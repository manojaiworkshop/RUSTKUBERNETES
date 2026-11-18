# Testing guide (through Phase 6b)

Every command here has been run against the rustkubernetes binaries with the official `kubectl` (v1.35).

## 1. Automated tests

```bash
cd ~/Documents/KUBERNETES/rustkubernetes
make build          # build everything
make test           # 120 unit + integration tests (apiserver, auth, RBAC, controllers, scheduler, kubelet, proxy)
hack/test-cmd.sh    # 39 checks with the official kubectl against the real binaries (hollow nodes)
make test-e2e-node  # 59 checks on a secure cluster: pods, Services, RBAC, tokens, kubelet auth, CoreDNS
make verify         # fmt + clippy + all tests + kubectl checks
```

## 2. Start a cluster

Terminal 1 (Ctrl-C stops everything):

```bash
hack/local-up-cluster.sh                 # secure cluster: TLS, RBAC, CoreDNS + all components
SECURE=0 hack/local-up-cluster.sh        # old insecure HTTP mode (no auth, no DNS add-on)
KUBE_PROXY=0 hack/local-up-cluster.sh    # without kube-proxy (leaves the host firewall alone)
CLEAN=1 hack/local-up-cluster.sh         # start from an empty cluster
HOLLOW_NODES=2 hack/local-up-cluster.sh  # real node + 2 hollow nodes
KUBELET=0 HOLLOW_NODES=5 HOLLOW_CPU=2 HOLLOW_ZONES=2 hack/local-up-cluster.sh   # no Docker needed
```

Terminal 2:

```bash
export KUBECONFIG=$PWD/_output/local/kubeconfig
alias rk=./target/debug/rkubectl         # optional: the Rust kubectl
```

Logs: `_output/local/logs/{kube-apiserver,kube-controller-manager,kube-scheduler,kubelet,kube-proxy,kubemark}.log`.

The kubelet needs Docker access (`docker ps` works without sudo). Its containers are visible with
`docker ps --filter label=io.rustkubernetes.node=$(hostname)`.

> **Hollow nodes** are kubemark fake kubelets: pods bound to them become `Running`/`Ready` with an IP, but no
> container really runs (no `logs`/`exec`). Use them for scheduling experiments; pin a pod to the real node with
> `nodeName` or `nodeSelector: {kubernetes.io/hostname: <node>}`.

## 3. Cluster basics

```bash
kubectl version
kubectl get nodes -o wide -L topology.kubernetes.io/zone
kubectl get lease -n kube-system          # kube-controller-manager and kube-scheduler leaders
kubectl get lease -n kube-node-lease      # node heartbeats
kubectl api-resources
kubectl get priorityclasses
```

## 3a. Real containers (kubelet)

```bash
kubectl run web --image=nginx:1.27-alpine
kubectl get pod web -o wide                       # Running, IP 172.17.x.x, node = your host
kubectl logs web ; kubectl logs -f web --tail=5   # Ctrl-C to stop following
kubectl exec web -- nginx -v
kubectl exec -it web -- sh                        # interactive shell (TTY, resize)
kubectl exec web -- sh -c 'exit 3'; echo $?       # → 3
curl -s http://$(kubectl get pod web -o jsonpath='{.status.podIP}') | head -4   # reach the pod directly

kubectl run crash --image=busybox:1.36 -- sh -c 'echo boom; exit 1'
kubectl get pod crash -w                          # Error → CrashLoopBackOff, RESTARTS grows (10s, 20s, 40s…)
kubectl logs crash --previous

kubectl create deploy hello --image=nginx:1.27-alpine --replicas=3
kubectl rollout status deploy/hello
kubectl create job pi --image=busybox:1.36 -- sh -c 'echo computing; sleep 2; echo done'
kubectl wait --for=condition=complete job/pi && kubectl logs job/pi
kubectl describe pod web                          # Scheduled / Pulled / Created / Started events
kubectl get node $(hostname) -o yaml | less        # real capacity, nodeInfo, images
```

Volumes, env vars and probes — save as `demo.yaml` and `kubectl apply -f demo.yaml`:

```yaml
apiVersion: v1
kind: ConfigMap
metadata: {name: demo}
data: {greeting: hello}
---
apiVersion: v1
kind: Pod
metadata: {name: demo, labels: {app: demo}}
spec:
  initContainers:
  - {name: init, image: busybox:1.36, command: [sh, -c, "date > /work/started"], volumeMounts: [{name: work, mountPath: /work}]}
  containers:
  - name: app
    image: nginx:1.27-alpine
    env:
    - {name: GREETING, valueFrom: {configMapKeyRef: {name: demo, key: greeting}}}
    - {name: POD_NAME, valueFrom: {fieldRef: {fieldPath: metadata.name}}}
    - {name: MSG, value: "$(GREETING) from $(POD_NAME)"}
    resources: {limits: {cpu: 500m, memory: 128Mi}}
    readinessProbe: {httpGet: {path: /, port: 80}, periodSeconds: 2}
    livenessProbe: {tcpSocket: {port: 80}}
    volumeMounts:
    - {name: cfg, mountPath: /etc/demo}
    - {name: work, mountPath: /work}
  volumes:
  - {name: cfg, configMap: {name: demo}}
  - {name: work, emptyDir: {}}
```

```bash
kubectl exec demo -- sh -c 'echo $MSG; cat /etc/demo/greeting /work/started; cat /sys/fs/cgroup/memory.max'
kubectl edit configmap demo        # change greeting; /etc/demo/greeting updates within a few seconds
```

Kubelet restart: stop the kubelet process — containers keep running; start it again and it adopts them.

## 3b. Services (kube-proxy)

```bash
kubectl create deploy hello --image=nginx:1.27-alpine --replicas=3
kubectl rollout status deploy/hello
# make each replica answer with its own name
for p in $(kubectl get pods -l app=hello -o name); do kubectl exec ${p#pod/} -- sh -c "echo ${p#pod/} > /usr/share/nginx/html/index.html"; done
kubectl expose deploy hello --port=80 --type=NodePort
CIP=$(kubectl get svc hello -o jsonpath='{.spec.clusterIP}'); NP=$(kubectl get svc hello -o jsonpath='{.spec.ports[0].nodePort}')

for i in $(seq 9); do curl -s $CIP; done | sort | uniq -c        # spread over the 3 pods
curl -s http://$(hostname -I | awk '{print $1}'):$NP              # NodePort on the node IP
kubectl run client --image=busybox:1.36 -- sleep 3600
kubectl exec client -- wget -qO- http://$CIP                       # from a pod
kubectl exec client -- sh -c 'echo $HELLO_SERVICE_HOST:$HELLO_SERVICE_PORT'   # service env vars

kubectl patch svc hello -p '{"spec":{"sessionAffinity":"ClientIP"}}'
for i in $(seq 9); do curl -s $CIP; done | sort | uniq -c        # always the same pod
kubectl patch svc hello -p '{"spec":{"externalTrafficPolicy":"Local"}}'   # pods see the real client IP

kubectl scale deploy hello --replicas=0; sleep 3; curl -v $CIP    # "Connection refused" at once (no endpoints)

curl -s 127.0.0.1:10256/healthz; curl -s 127.0.0.1:10249/metrics   # kube-proxy health and metrics
docker exec rustkube-proxy-$(hostname | tr A-Z a-z) nft list table ip rustkube-proxy | less   # the rules
```

## 3c. Security and DNS (Phase 6)

```bash
kubectl auth whoami                                   # kubernetes-admin, groups kubeadm:cluster-admins
curl -sk https://172.17.0.1:6443/readyz               # anonymous: allowed
curl -sk https://172.17.0.1:6443/api                  # anonymous: 403 (discovery needs authentication)
kubectl get clusterroles | head; kubectl describe clusterrole edit | head -20

# A new user (O = groups) and RBAC
./target/debug/kubeadm kubeconfig user --client-name jane --org dev \
   --cert-dir _output/local/pki --control-plane-endpoint https://172.17.0.1:6443 > jane.conf
kubectl --kubeconfig jane.conf get pods                         # Forbidden
kubectl create ns dev
kubectl create rolebinding dev-edit --clusterrole=edit --group=dev -n dev
kubectl --kubeconfig jane.conf auth can-i create deployments -n dev       # yes
kubectl --kubeconfig jane.conf auth can-i --list -n dev
kubectl auth can-i delete pods -n dev --as jane --as-group dev            # impersonation
kubectl create rolebinding dev-admin --clusterrole=admin --group=dev -n dev
kubectl --kubeconfig jane.conf create role sneaky --verb=get --resource=nodes -n dev   # escalation refused

# ServiceAccount tokens
kubectl create sa ci -n dev && kubectl create rolebinding ci-view --clusterrole=view --serviceaccount=dev:ci -n dev
TOKEN=$(kubectl create token ci -n dev)
kubectl --kubeconfig /dev/null --server https://172.17.0.1:6443 --certificate-authority _output/local/pki/ca.crt \
   --token "$TOKEN" get pods -n dev
kubectl run curl --image=curlimages/curl:8.11.1 --command -- sleep 3600
kubectl exec curl -- ls /var/run/secrets/kubernetes.io/serviceaccount     # ca.crt namespace token
kubectl exec curl -- sh -c 'curl -s --cacert /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
   -H "Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)" \
   https://kubernetes.default.svc/api/v1/namespaces/default/pods'           # 403 until you bind a role

# Cluster DNS
kubectl get pods -n kube-system -l k8s-app=kube-dns
kubectl create deploy web --image=nginx:1.27-alpine && kubectl expose deploy web --port=80
kubectl run client --image=busybox:1.36 -- sleep 3600
kubectl exec client -- cat /etc/resolv.conf
kubectl exec client -- nslookup web.default.svc.cluster.local
kubectl exec client -- wget -qO- http://web
```

## 4. Workloads

```bash
kubectl create deploy web --image=nginx:1.27 --replicas=6
kubectl rollout status deploy/web         # completes
kubectl get pods -o wide                  # spread over the nodes, with IPs
kubectl set image deploy/web nginx=nginx:1.28 && kubectl rollout status deploy/web
kubectl rollout history deploy/web
kubectl rollout undo deploy/web
kubectl scale deploy/web --replicas=2
kubectl delete pod <one-web-pod>          # replaced automatically

kubectl create job pi --image=perl -- perl -e 1
kubectl wait --for=condition=complete job/pi --timeout=60s
kubectl create cronjob tick --image=busybox --schedule="*/1 * * * *" -- date
kubectl get jobs -w

kubectl apply -f examples/guestbook.yaml
kubectl rollout status deploy/frontend -n guestbook
```

StatefulSet (pods are created in order `db-0`, `db-1`, `db-2`):

```bash
kubectl apply -f - <<'EOF'
apiVersion: apps/v1
kind: StatefulSet
metadata: {name: db}
spec:
  serviceName: db
  replicas: 3
  selector: {matchLabels: {app: db}}
  template:
    metadata: {labels: {app: db}}
    spec: {containers: [{name: pg, image: postgres:16}]}
EOF
kubectl get pods -l app=db -w
```

DaemonSet (one pod per node):

```bash
kubectl apply -f - <<'EOF'
apiVersion: apps/v1
kind: DaemonSet
metadata: {name: agent, namespace: kube-system}
spec:
  selector: {matchLabels: {app: agent}}
  template:
    metadata: {labels: {app: agent}}
    spec: {containers: [{name: a, image: fluent-bit}]}
EOF
kubectl get pods -n kube-system -o wide -l app=agent
```

## 5. Services

```bash
kubectl expose deploy web --port 80 --type=NodePort
kubectl get svc web
kubectl get endpoints web                 # pod IPs:ports
kubectl get endpointslices
```

## 6. Scheduling

```bash
# Unschedulable → reason in events
kubectl run huge --image=nginx --overrides='{"spec":{"containers":[{"name":"huge","image":"nginx","resources":{"requests":{"cpu":"64"}}}]}}'
kubectl describe pod huge | tail -3       # 0/3 nodes are available: 3 Insufficient cpu.

# nodeSelector
kubectl label node hollow-node-2 disk=ssd
kubectl run fast --image=nginx --overrides='{"spec":{"nodeSelector":{"disk":"ssd"}}}'
kubectl get pod fast -o wide

# taints, cordon, drain
kubectl taint node hollow-node-0 dedicated=gpu:NoSchedule
kubectl cordon hollow-node-1 && kubectl uncordon hollow-node-1
kubectl drain hollow-node-1 --ignore-daemonsets --force
kubectl taint node hollow-node-0 dedicated-          # remove the taint

# anti-affinity: at most one replica per node
kubectl apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: {name: spread}
spec:
  replicas: 4
  selector: {matchLabels: {app: spread}}
  template:
    metadata: {labels: {app: spread}}
    spec:
      affinity:
        podAntiAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
          - {labelSelector: {matchLabels: {app: spread}}, topologyKey: kubernetes.io/hostname}
      containers: [{name: c, image: nginx}]
EOF
kubectl get pods -l app=spread -o wide    # 3 running (one per node), 1 Pending
```

Priority and preemption (start the cluster with `HOLLOW_CPU=2`):

```bash
kubectl create priorityclass high --value=1000
kubectl create priorityclass low --value=10
kubectl apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: {name: filler}
spec:
  replicas: 3
  selector: {matchLabels: {app: filler}}
  template:
    metadata: {labels: {app: filler}}
    spec:
      priorityClassName: low
      containers: [{name: c, image: nginx, resources: {requests: {cpu: 1500m}}}]
EOF
kubectl rollout status deploy/filler
kubectl run vip --image=nginx --overrides='{"spec":{"priorityClassName":"high","containers":[{"name":"vip","image":"nginx","resources":{"requests":{"cpu":"1500m"}}}]}}'
kubectl get pods -o wide                  # vip Running, one filler pod Pending
kubectl get events --field-selector reason=Preempted
```

Scheduling gates:

```bash
kubectl run gated --image=nginx --overrides='{"spec":{"schedulingGates":[{"name":"example.com/wait"}]}}'
kubectl get pod gated                     # Pending, not scheduled
kubectl patch pod gated --type=json -p '[{"op":"remove","path":"/spec/schedulingGates"}]'
kubectl get pod gated -o wide             # scheduled
```

## 7. Garbage collection, finalizers, namespaces

```bash
kubectl delete deploy web                          # ReplicaSets + pods removed
kubectl create deploy keep --image=nginx
kubectl delete deploy keep --cascade=orphan        # ReplicaSet survives
kubectl create ns shop && kubectl get sa -n shop   # default SA created
kubectl delete ns shop                             # content deleted first
kubectl delete ns kube-system                      # refused
```

## 8. Node failure

Stop hollow nodes only (find the kubemark process and stop it), or run a separate cluster with `HOLLOW_NODES=0`
and start `./target/debug/kubemark --kubeconfig $KUBECONFIG --nodes 2` by hand, then press Ctrl-C:

```bash
kubectl get nodes -w                      # NotReady/Unknown after ~40s; unreachable taints added
```

## 9. Persistence

Stop the cluster (Ctrl-C) and start it again without `CLEAN=1`: `kubectl get all -A` shows everything again.

## 10. Not available yet

| Missing | Arrives in |
|---------|------------|
| etcd storage | Phase 6c |
| containerd runtime, `kubectl explain`, metrics, audit | Phase 6d |
| `kubectl attach/port-forward/cp`, multi-host pod networking (CNI) | later |
| TLS, users, RBAC, `kubectl explain` schemas | Phase 6 |
