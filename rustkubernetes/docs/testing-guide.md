# Testing guide (through Phase 6j)

Every command here has been run against the rustkubernetes binaries with the official `kubectl` (v1.35).

## 1. Automated tests

```bash
cd ~/Documents/KUBERNETES/rustkubernetes
make build          # build everything
make test           # 195 unit + integration tests (apiserver, auth, RBAC, controllers, scheduler, kubelet, proxy)
hack/test-cmd.sh    # 54 checks with the official kubectl against the real binaries (hollow nodes)
make test-etcd      # storage conformance + kubectl suite against a real etcd
make test-e2e-network  # two machines: node IPAM, kubenet, flannel, cross-node pods/Services/DNS
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

## 3d. etcd (Phase 6c)

```bash
hack/local-up-cluster.sh                    # etcd runs in a container (data in _output/local/etcd)
ENCRYPT_SECRETS=1 hack/local-up-cluster.sh  # also encrypt Secrets at rest
kubectl get --raw '/readyz?verbose'         # [+]etcd ok
EC="docker exec rustkube-etcd-$(hostname | tr A-Z a-z) etcdctl --endpoints https://127.0.0.1:2379 \
  --cacert /etc/kubernetes/pki/etcd/ca.crt --cert /etc/kubernetes/pki/etcd/healthcheck-client.crt \
  --key /etc/kubernetes/pki/etcd/healthcheck-client.key"
$EC get --prefix /registry --keys-only | head          # /registry/minions/…, /registry/services/specs/…
kubectl create secret generic demo --from-literal=pw=s3cret
$EC get /registry/secrets/default/demo --print-value-only | head -c 40   # k8s:enc:aesgcm:v1:key1:… (encrypted)
# Stop the cluster (Ctrl-C) and start it again: everything is still there.
```

## 3e. Pod network across machines (Phase 6d)

```bash
make test-e2e-network                       # 36 checks, vxlan; BACKEND=host-gw for the other backend
KEEP=1 ID=dev hack/test-e2e-network.sh      # leave the two machines running to explore
export KUBECONFIG=$(pwd)/_output/test-e2e-network/run-dev/admin.conf
kubectl get nodes -o custom-columns=NAME:.metadata.name,CIDR:.spec.podCIDR
kubectl get pods -A -o wide                 # flannel on n1 and n2
docker exec rk-dev-n1 ip route             # 10.244.x.0/24 via ... dev flannel.1
ID=dev hack/test-e2e-network.sh --cleanup
```

On a real multi-machine cluster (rustkubeadmin), pods get IPs from each node's `spec.podCIDR`. Check with:

```bash
kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"  "}{.spec.podCIDR}{"\n"}{end}'
kubectl run a --image=busybox:1.36 --overrides='{"spec":{"nodeName":"<node-1>"}}' -- sleep 3600
kubectl run b --image=busybox:1.36 --overrides='{"spec":{"nodeName":"<node-2>"}}' -- sleep 3600
kubectl exec a -- ping -c 3 $(kubectl get pod b -o jsonpath='{.status.podIP}')
```

## 3f. Node authorization (Phase 6e)

Act as the kubelet with its own kubeconfig (local-up writes `_output/local/kubelet.conf`):

```bash
KL="kubectl --kubeconfig _output/local/kubelet.conf"
$KL get node $(hostname | tr A-Z a-z)          # its own Node: allowed
$KL get nodes                                   # forbidden: cannot read all nodes, only its own Node object
kubectl create secret generic unrelated --from-literal=x=y
$KL get secret unrelated                        # forbidden: no relationship found between node ... and this object
$KL label node $(hostname | tr A-Z a-z) node-role.kubernetes.io/worker=   # NodeRestriction: not allowed to modify labels
kubectl get clusterrolebinding system:node -o yaml                          # no subjects
```

## 3g. port-forward, attach, cp (Phase 6f)

```bash
kubectl run web --image=nginx:1.27-alpine && kubectl wait --for=condition=Ready pod/web
kubectl port-forward pod/web 8080:80 &      # Forwarding from 127.0.0.1:8080 -> 80
curl -s localhost:8080 | head -4
KUBECTL_PORT_FORWARD_WEBSOCKETS=false kubectl port-forward pod/web 8081:80 &   # plain SPDY
kubectl run -it --rm shell --image=busybox:1.36 -- sh                           # attach with a TTY
kubectl cp /etc/hostname web:/tmp/hostname && kubectl exec web -- cat /tmp/hostname
KUBECTL_REMOTE_COMMAND_WEBSOCKETS=false kubectl exec -it web -- sh              # exec over SPDY
```

## 3h. Resource metrics (Phase 6g)

`hack/local-up-cluster.sh` deploys metrics-server with CoreDNS (needs kube-proxy). Give it ~30 s for two scrapes:

```bash
kubectl get apiservices                       # v1beta1.metrics.k8s.io   kube-system/metrics-server   True
kubectl top nodes
kubectl top pods -A
kubectl get --raw /apis/metrics.k8s.io/v1beta1/nodes | head -c 300
P=_output/local/pki
curl -s --cacert $P/ca.crt --cert $P/apiserver-kubelet-client.crt --key $P/apiserver-kubelet-client.key \
  https://$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Gateway}}'):10250/metrics/resource | head
```

## 3i. Persistent volumes (Phase 6h)

`hack/local-up-cluster.sh` enables the host-path provisioner and the `standard (default)` class:

```bash
kubectl get sc                                   # standard (default)   kubernetes.io/host-path   Delete   Immediate
kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: data}
spec: {accessModes: [ReadWriteOnce], resources: {requests: {storage: 100Mi}}}
---
apiVersion: v1
kind: Pod
metadata: {name: writer}
spec:
  containers: [{name: app, image: busybox:1.36, command: [sh, -c, "date > /data/now; sleep 3600"], volumeMounts: [{name: d, mountPath: /data}]}]
  volumes: [{name: d, persistentVolumeClaim: {claimName: data}}]
EOF
kubectl get pvc,pv                               # Bound, pvc-<uid>, RWO, standard
kubectl delete pvc data --wait=false; kubectl get pvc data   # Terminating while the pod uses it
kubectl delete pod writer                        # then the claim and (Delete policy) the volume go
```

On console clusters the default class is `local-path` (WaitForFirstConsumer): the claim binds when its pod is
scheduled, to a directory on that pod's node.

## 3j. Certificates (Phase 6i)

```bash
# A client certificate for "jane" through the CSR API
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout jane.key -subj "/O=dev/CN=jane" -out jane.csr
kubectl apply -f - <<YAML
apiVersion: certificates.k8s.io/v1
kind: CertificateSigningRequest
metadata: {name: jane}
spec: {request: $(base64 -w0 < jane.csr), signerName: kubernetes.io/kube-apiserver-client, usages: [client auth], expirationSeconds: 86400}
YAML
kubectl get csr                       # jane  ...  kubernetes.io/kube-apiserver-client  kubernetes-admin  24h  Pending
kubectl certificate approve jane      # → Approved,Issued
kubectl get csr jane -o jsonpath='{.status.certificate}' | base64 -d > jane.crt

# Bootstrap tokens and joining a node
kubeadm token create --print-join-command --kubeconfig _output/local-up-cluster/admin.conf
kubeadm token list --kubeconfig _output/local-up-cluster/admin.conf
kubeadm join HOST:PORT --token TOKEN --discovery-token-ca-cert-hash sha256:HASH --kubeconfig-dir /tmp/node2
kubelet --bootstrap-kubeconfig /tmp/node2/bootstrap-kubelet.conf --kubeconfig /tmp/node2/kubelet.conf --rotate-certificates ...

# Certificate lifetimes
kubeadm certs check-expiration --cert-dir _output/local-up-cluster/pki --kubeconfig-dir _output/local-up-cluster
kubeadm certs renew all --cert-dir ... --kubeconfig-dir ...   # then restart the control plane
```

## 3k. CustomResourceDefinitions (Phase 6j)

```bash
kubectl apply -f examples/crontab-crd.yaml
kubectl wait --for condition=established crd/crontabs.stable.example.com
kubectl get crd                                   # NAME  CREATED AT
kubectl api-resources --api-group=stable.example.com
kubectl apply -f examples/crontab.yaml
kubectl get ct                                    # SPEC  REPLICAS  AGE columns; replicas defaulted to 1
kubectl get ct my-new-cron-object -o yaml
kubectl patch ct my-new-cron-object --type=merge -p '{"spec":{"replicas":50}}'
# → spec.replicas: Invalid value: 50: spec.replicas in body should be less than or equal to 10
kubectl apply -f - <<'YAML'                       # → strict decoding error: unknown field "spec.foo"
apiVersion: stable.example.com/v1
kind: CronTab
metadata: {name: x}
spec: {foo: 1}
YAML
kubectl scale ct my-new-cron-object --replicas=3
kubectl patch ct my-new-cron-object --subresource=status --type=merge -p '{"status":{"replicas":3}}'
kubectl get --raw /apis/stable.example.com/v1/namespaces/default/crontabs/my-new-cron-object/scale
kubectl explain crontab.spec
kubectl get apiservices | grep example            # v1.stable.example.com  Local
kubectl delete crd crontabs.stable.example.com    # deletes the instances first
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
| `kubectl explain` for built-in types, audit | next |
| containerd runtime, `kubectl explain`, metrics, audit | Phase 6d |
| TLS, users, RBAC, `kubectl explain` schemas | Phase 6 |
