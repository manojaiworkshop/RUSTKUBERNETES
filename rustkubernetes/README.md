# rustkubernetes

A from-scratch reimplementation of Kubernetes in **Rust**, built phase by phase. The repository layout mirrors
[kubernetes/kubernetes](../kubernetes), so every Rust module has an obvious Go counterpart.

The goal is **wire compatibility**: the official `kubectl` (and other client-go based tools) talk to the
rustkubernetes API server exactly as they would to a real cluster.

```console
$ hack/local-up-cluster.sh            # terminal 1
$ export KUBECONFIG=$PWD/_output/local/kubeconfig
$ kubectl apply -f examples/guestbook.yaml
namespace/guestbook created
configmap/frontend-config created
deployment.apps/redis-leader created
service/redis-leader created
deployment.apps/frontend created
service/frontend created
$ kubectl get svc -n guestbook
NAME           TYPE        CLUSTER-IP   EXTERNAL-IP   PORT(S)        AGE
frontend       NodePort    10.96.0.3    <none>        80:30000/TCP   4s
redis-leader   ClusterIP   10.96.0.2    <none>        6379/TCP       4s
```

## Status

| Phase | Component(s) | Status |
|-------|--------------|--------|
| **1** | apimachinery, api types, apiserver library, **kube-apiserver**, client-go, kubectl (`rkubectl`) | ✅ done |
| **2** | **kube-controller-manager**: 15 controllers (ReplicaSet, Deployment, StatefulSet, DaemonSet, Job, CronJob, TTL, Namespace, GC, ServiceAccount, Endpoints, EndpointSlice, NodeLifecycle, TaintEviction, PodGC), leader election | ✅ done |
| **3** | **kube-scheduler**: queue, cache, framework, 11 plugins, preemption, profiles; PriorityClass + Priority admission; **kubemark hollow nodes** | ✅ done |
| **4** | **kubelet** (Docker runtime, cri-dockerd style): pod workers, PLEG, sandboxes, init containers, restart policies + CrashLoopBackOff, probes, volumes, env, node status + lease; API server `pods/log` + `pods/exec` proxy | ✅ done |
| **5** | **kube-proxy** (nftables mode): ClusterIP, NodePort, externalIPs/LB IPs, session affinity, traffic policies, hairpin, UDP conntrack cleanup, health + metrics | ✅ done |
| **6a** | **Security**: `kubeadm` PKI + kubeconfigs, TLS everywhere, x509 / token / ServiceAccount JWT authentication, RBAC (+ bootstrap policy, escalation prevention), impersonation, access/token reviews, TokenRequest + projected tokens, authenticated kubelet API | ✅ done |
| **6b** | **Cluster DNS**: CoreDNS add-on, kubelet `dnsPolicy` / `--cluster-dns` | ✅ done |
| **UI** | **rustkubeadmin** web console (`admin/`): super admin from env, users with console permissions and per-namespace RBAC (via impersonation), machines over SSH with an offline installer, cluster create/import/start/stop/delete, live nodes/pods/workloads | ✅ done |
| **6c** | **etcd storage**: etcd v3 gRPC client, upstream store semantics and key layout, compaction, `etcd` health checks, kubeadm etcd PKI + flags, encryption at rest (`EncryptionConfiguration`, aesgcm) | ✅ done |
| **6d** | **Pod network across machines**: node IPAM (`--allocate-node-cidrs`), kubelet `--network-plugin=kubenet` (bridge `cbr0` on `spec.podCIDR`, NetworkReady), flannel add-on (vxlan / host-gw, `--ip-masq`) | ✅ done |
| **6e** | **Node authorization**: Node authorizer (graph of pods → secrets/configmaps/PVCs/ServiceAccounts, own Node/Lease/pods), NodeRestriction admission, `--enable/disable-admission-plugins` | ✅ done |
| **6f** | **Streaming**: `kubectl port-forward` (SPDY/3.1 via WebSocket tunnel or direct, into the pod's network namespace), `attach`, `run -i`, `cp`, exec/attach over SPDY | ✅ done |
| **6g** | **Resource metrics**: kubelet `/metrics/resource`, aggregation layer (`APIService`, availability, front-proxy proxying, `extension-apiserver-authentication`), metrics-server add-on, `kubectl top` | ✅ done |
| **6h** | **Persistent volumes**: PV/PVC/StorageClass, `DefaultStorageClass` + `StorageObjectInUseProtection`, PV binder (binding, host-path & external provisioning, reclaim), protection and ephemeral-volume controllers, scheduler `VolumeBinding` (`WaitForFirstConsumer`), kubelet PVC mounts, local-path-provisioner add-on | ✅ done |
| **6i** | **Certificates**: `certificates.k8s.io/v1` CSRs (`kubectl certificate approve/deny`), signing/approving/cleaner controllers, CSR admission plugins, bootstrap tokens (`--enable-bootstrap-token-auth`, bootstrap-signer, token-cleaner), kubelet TLS bootstrap + client/serving certificate rotation, `kubeadm token`, `kubeadm join` discovery, `kubeadm certs check-expiration/renew` | ✅ done |
| **6j** | **CustomResourceDefinitions**: `apiextensions.k8s.io/v1` CRDs (validation, naming, `Established`), structural schemas (validation, defaulting, pruning), `fieldValidation` + `Warning` headers, `status`/`scale` subresources, printer columns, multi-version with `None`/`Webhook` conversion, `kubectl explain` for custom resources, cleanup finalizer, GC of custom resources | ✅ done |
| next | Ingress/NetworkPolicy/HPA/ResourceQuota/LimitRange/PDB, HA control plane, audit, API Priority & Fairness, containerd, conformance | planned |

See [docs/roadmap.md](docs/roadmap.md) for the detailed plan, [docs/phase-1-core.md](docs/phase-1-core.md) and
[docs/phase-2-controllers.md](docs/phase-2-controllers.md), [docs/phase-3-scheduler.md](docs/phase-3-scheduler.md),
[docs/phase-4-kubelet.md](docs/phase-4-kubelet.md), [docs/phase-5-kube-proxy.md](docs/phase-5-kube-proxy.md),
[docs/phase-6-security.md](docs/phase-6-security.md), [docs/phase-6c-etcd.md](docs/phase-6c-etcd.md),
[docs/phase-6d-pod-network.md](docs/phase-6d-pod-network.md), [docs/phase-6e-node-authorization.md](docs/phase-6e-node-authorization.md), [docs/phase-6f-streaming.md](docs/phase-6f-streaming.md), [docs/phase-6g-metrics.md](docs/phase-6g-metrics.md), [docs/phase-6h-storage.md](docs/phase-6h-storage.md), [docs/phase-6i-certificates.md](docs/phase-6i-certificates.md), [docs/phase-6j-crds.md](docs/phase-6j-crds.md) for
what each phase implements and how, and **[docs/testing-guide.md](docs/testing-guide.md)** for hands-on commands.

## Repository layout

```
rustkubernetes/
├── cmd/                              binaries            (k8s: cmd/)
│   ├── kube-apiserver/               kube-apiserver
│   ├── kube-controller-manager/      kube-controller-manager
│   ├── kube-scheduler/               kube-scheduler
│   ├── kubelet/                      kubelet (node agent, Docker runtime)
│   ├── kube-proxy/                   kube-proxy (Service proxy, nftables)
│   ├── kubeadm/                      kubeadm (certificates + kubeconfigs)
│   ├── kubemark/                     hollow nodes (fake kubelets for testing)
│   └── kubectl/                      rkubectl
├── pkg/                              core implementation (k8s: pkg/)
│   ├── apis/                         defaulting + validation per API group     (pkg/apis)
│   ├── registry/                     per-resource REST strategies, IP/port allocators (pkg/registry)
│   ├── controlplane/                 assembles the API server, bootstrap       (pkg/controlplane)
│   ├── controller/                   the 15 built-in controllers               (pkg/controller)
│   ├── scheduler/                    queue, cache, framework, plugins           (pkg/scheduler)
│   ├── kubelet/                      pod workers, dockershim, probes, volumes   (pkg/kubelet)
│   ├── proxy/                        Service proxy: nftables rules, endpoints   (pkg/proxy)
│   ├── serviceaccount/               ServiceAccount JWTs (issue / verify)       (pkg/serviceaccount)
│   └── kubemark/                     hollow node implementation                 (pkg/kubemark)
├── plugin/                           admission plugins   (k8s: plugin/pkg/admission)
├── staging/src/k8s.io/               published libraries (k8s: staging/src/k8s.io)
│   ├── apimachinery/                 meta types, errors, selectors, validation
│   ├── api/                          typed API objects + protobuf wire decoding
│   │   └── proto/                    upstream generated.proto files (Apache-2.0)
│   ├── apiserver/                    generic API server: storage, watch, handlers, admission, discovery, OpenAPI
│   └── client-go/                    REST client, typed Api<T>, kubeconfig, informers, workqueue,
│                                     leader election, events, discovery
├── vendor/go.etcd.io/etcd/client/v3/  etcd v3 gRPC client from etcd's protos (k8s: vendor/)
├── staging/src/k8s.io/component-helpers/ storage/volume helpers (PV matching and binding)
├── staging/src/k8s.io/kube-aggregator/ aggregation layer (APIService, proxy to extension API servers)
├── staging/src/k8s.io/apiextensions-apiserver/ CustomResourceDefinitions (schemas, conversion, CRD controllers)
├── vendor/github.com/moby/spdystream/ SPDY/3.1 (port-forward, exec/attach for older clients)
├── test/integration/                 in-process API server tests (k8s: test/integration)
├── admin/                            rustkubeadmin web console: Rust API + Next.js UI (docs/admin-console.md)
├── cluster/addons/dns/coredns/       CoreDNS add-on manifest (k8s: cluster/addons)
├── build/kube-proxy/                 runtime image for kube-proxy (nft, conntrack) (k8s: build/)
├── hack/                             local-up-cluster.sh, test-cmd.sh, test-e2e-node.sh, verify.sh (k8s: hack/)
├── examples/                         sample manifests
└── docs/                             design docs and roadmap
```

| Rust crate | Path | Go module / package |
|------------|------|---------------------|
| `k8s-apimachinery` | `staging/src/k8s.io/apimachinery` | `k8s.io/apimachinery` |
| `k8s-api` | `staging/src/k8s.io/api` | `k8s.io/api` |
| `k8s-apiserver` | `staging/src/k8s.io/apiserver` | `k8s.io/apiserver` |
| `k8s-client-go` | `staging/src/k8s.io/client-go` | `k8s.io/client-go` |
| `kube-apis` | `pkg/apis` | `k8s.io/kubernetes/pkg/apis` |
| `kube-registry` | `pkg/registry` | `k8s.io/kubernetes/pkg/registry` |
| `kube-controlplane` | `pkg/controlplane` | `k8s.io/kubernetes/pkg/controlplane` |
| `kube-controller` | `pkg/controller` | `k8s.io/kubernetes/pkg/controller` |
| `kube-scheduler-core` | `pkg/scheduler` | `k8s.io/kubernetes/pkg/scheduler` |
| `kube-kubelet` | `pkg/kubelet` | `k8s.io/kubernetes/pkg/kubelet` |
| `kube-proxy-core` | `pkg/proxy` | `k8s.io/kubernetes/pkg/proxy` |
| `kube-serviceaccount` | `pkg/serviceaccount` | `k8s.io/kubernetes/pkg/serviceaccount` |
| `kube-kubemark` | `pkg/kubemark` | `k8s.io/kubernetes/pkg/kubemark` |
| `etcd-clientv3` | `vendor/go.etcd.io/etcd/client/v3` | `go.etcd.io/etcd/client/v3` |
| `kube-plugin` | `plugin` | `k8s.io/kubernetes/plugin` |
| `kube-apiserver` (bin) | `cmd/kube-apiserver` | `cmd/kube-apiserver` |
| `kube-controller-manager` (bin) | `cmd/kube-controller-manager` | `cmd/kube-controller-manager` |
| `kube-scheduler` (bin) | `cmd/kube-scheduler` | `cmd/kube-scheduler` |
| `kubelet` (bin) | `cmd/kubelet` | `cmd/kubelet` |
| `kube-proxy` (bin) | `cmd/kube-proxy` | `cmd/kube-proxy` |
| `kubeadm` (bin) | `cmd/kubeadm` | `cmd/kubeadm` |
| `rustkube-admin` (bin `rustkubeadmin`) | `admin` | — (web console, like pgrustadmin) |
| `kubemark` (bin) | `cmd/kubemark` | `cmd/kubemark` |
| `rkubectl` (bin) | `cmd/kubectl` | `cmd/kubectl` |

## Quick start

Requirements: Rust ≥ 1.80 (`cargo`). For real pods and Services: Docker, with your user in the `docker` group
(kube-proxy also needs the `br_netfilter` module, loaded by Docker on most hosts).
Optional: the official `kubectl` for compatibility testing.

```bash
make build                 # or: cargo build
make test                  # unit + integration tests (185 tests)
make verify                # fmt + clippy + tests + kubectl command tests
make test-etcd             # storage conformance + kubectl suite against a real etcd
make test-e2e-network      # two machines (docker-in-docker): node IPAM, kubenet, flannel, cross-node traffic
make test-e2e-node         # secure cluster: pods, logs, exec, volumes, probes, Services, RBAC, tokens, DNS (59 checks)
hack/local-up-cluster.sh   # secure cluster (TLS, RBAC, CoreDNS): apiserver + KCM + scheduler + kubelet + kube-proxy
SECURE=0 hack/local-up-cluster.sh                   # old insecure HTTP dev mode
KUBE_PROXY=0 hack/local-up-cluster.sh               # without kube-proxy (no host firewall changes)
make admin && RUSTKUBEADMIN_USER=admin RUSTKUBEADMIN_PASSWORD='change-me-please' make run-admin   # web console on :8090
KUBELET=0 HOLLOW_NODES=3 hack/local-up-cluster.sh   # no Docker: 3 hollow nodes instead
```

Use the cluster with either CLI:

```bash
export KUBECONFIG=$PWD/_output/local/kubeconfig
kubectl get nodes -o wide
kubectl auth whoami                                   # kubernetes-admin (kubeadm:cluster-admins)
kubectl run web --image=nginx:1.27-alpine
kubectl logs -f web
kubectl exec -it web -- sh
kubectl expose pod web --port=80 --type=NodePort
curl http://$(kubectl get svc web -o jsonpath='{.spec.clusterIP}')    # Service traffic via kube-proxy
./target/debug/rkubectl get all -n guestbook
./target/debug/rkubectl scale deploy/frontend -n guestbook --replicas=5
```

Run the API server directly:

```bash
./target/debug/kube-apiserver --port 6443 --data-dir /tmp/rk8s-data \
    --service-cluster-ip-range 10.96.0.0/12 --service-node-port-range 30000-32767 --v 2
```

## What works with the official kubectl

`get` (tables, `-o wide|yaml|json|jsonpath|name`, `-l`, `--field-selector`, `-A`, `-w`), `apply` (client-side
and `--server-side`), `create` (from files and generators: `deployment`, `configmap`, `secret`, `service`, `sa`, ...),
`run`, `expose`, `delete` (incl. finalizers and namespace cascade), `patch` (json / merge / strategic), `label`,
`annotate`, `scale`, `set image`, `describe`, `diff`, `--dry-run=server`, `api-resources`, `version`.

With the controller-manager: Deployments create ReplicaSets and Pods, `rollout history`/`undo`/`restart`, Jobs,
CronJobs, cascading deletion (`--cascade=background|foreground|orphan`), namespace deletion, default
ServiceAccounts, Endpoints/EndpointSlices, events in `kubectl describe` / `kubectl get events`.

With the scheduler and hollow nodes: pods are scheduled (resources, selectors, affinity, taints, topology spread,
priorities and preemption), become Running/Ready with IPs, `kubectl rollout status` completes, Services get
endpoints, Jobs complete (`kubectl wait --for=condition=complete`).

With the kubelet: pods run as real Docker containers (pause sandbox + app containers sharing its network and IPC
namespaces), get bridge IPs, run init containers, restart per `restartPolicy` with CrashLoopBackOff, honour
liveness/readiness/startup probes, CPU/memory limits, configMap/secret/emptyDir/hostPath/downwardAPI/projected
volumes and env vars; `kubectl logs` (`-f`, `--previous`, `--tail`, `--timestamps`) and `kubectl exec`
(`-i`, `-t`, exit codes) work through the API server. The kubelet adopts running containers after a restart.

With kube-proxy: Service ClusterIPs, NodePorts, externalIPs and load-balancer IPs reach their pods (random load
balancing, `sessionAffinity: ClientIP`, `internal/externalTrafficPolicy: Local` with source-IP preservation,
hairpin traffic, immediate reject for Services without endpoints, UDP), from the host and from pods.

Kube-proxy runs in a host-network container with `NET_ADMIN`, and its rules live in their own nftables table
(`rustkube-proxy`), which is removed when the local cluster stops.

With Phase 6a/6b (default `hack/local-up-cluster.sh`): HTTPS with client certificates and ServiceAccount tokens;
RBAC with the default roles (`admin`, `edit`, `view`, ...) and escalation prevention; `kubectl auth can-i`
(`--list`), `kubectl auth whoami`, `kubectl --as`, `kubectl create token`, `kubectl create role/rolebinding`;
pods get auto-mounted, auto-rotated ServiceAccount tokens and can call the API at `https://kubernetes.default.svc`;
`kubectl logs/exec` go to an authenticated kubelet over TLS; CoreDNS resolves `svc.ns.svc.cluster.local`, short
names and external names.

With Phase 6c: state lives in etcd (gRPC, mutual TLS, upstream key layout, compaction), `/readyz` checks etcd, and
Secrets can be encrypted at rest with an `EncryptionConfiguration`.

With Phase 6d: every node gets a pod CIDR from kube-controller-manager, the kubelet's kubenet plugin puts pods on
`cbr0`, and the flannel add-on routes between machines, so pods, Services and NodePorts work across machines
(rustkubeadmin clusters use this by default).

With Phase 6e: kubelets are authorized by the Node authorizer (only the Secrets, ConfigMaps and tokens their own
pods use; only their own Node, Lease and pods) and NodeRestriction checks their writes.

With Phase 6f: `kubectl port-forward`, `attach`, `run -i`, `cp`, and exec/attach over SPDY for older clients.

With Phase 6g: `kubectl top nodes|pods` through metrics-server and the aggregation layer (`APIService`).

With Phase 6h: PersistentVolumeClaims bound to static or dynamically provisioned volumes (`WaitForFirstConsumer`
included), kept by reclaim policies and protection finalizers.

With Phase 6i: `kubectl get csr`, `kubectl certificate approve|deny`; kubelets renew their certificates;
`kubeadm token`, `kubeadm join`, `kubeadm certs check-expiration|renew`.

With Phase 6j: CustomResourceDefinitions — `kubectl apply -f crd.yaml`, custom resources with schema validation,
defaulting and pruning, `status`/`scale` subresources, printer columns, several versions, `kubectl explain` — so
operators and Helm charts that ship CRDs install.

Not yet (needs later phases): volume expansion and CSI, `explain` for built-in types (needs their OpenAPI
schemas), CNI plugins (pods use kubenet), certificate rotation.

## License

Apache License 2.0 (see [LICENSE](LICENSE)). The `.proto` files under `staging/src/k8s.io/api/proto` are copied
unchanged from the Kubernetes project (Apache-2.0).
