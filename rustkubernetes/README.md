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
| 6c | etcd v3 storage backend | ⏳ next |
| 6d | containerd CRI backend, OpenAPI schemas (`kubectl explain`), metrics, audit, API Priority & Fairness | planned |

See [docs/roadmap.md](docs/roadmap.md) for the detailed plan, [docs/phase-1-core.md](docs/phase-1-core.md) and
[docs/phase-2-controllers.md](docs/phase-2-controllers.md), [docs/phase-3-scheduler.md](docs/phase-3-scheduler.md),
[docs/phase-4-kubelet.md](docs/phase-4-kubelet.md), [docs/phase-5-kube-proxy.md](docs/phase-5-kube-proxy.md),
[docs/phase-6-security.md](docs/phase-6-security.md) for
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
make test                  # unit + integration tests (120 tests)
make verify                # fmt + clippy + tests + kubectl command tests
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

Not yet (needs later phases): etcd storage, `port-forward`, `attach`, `cp`, `top`, `explain` (needs OpenAPI
schemas), CNI networking across hosts, Node authorizer, certificate rotation.

## License

Apache License 2.0 (see [LICENSE](LICENSE)). The `.proto` files under `staging/src/k8s.io/api/proto` are copied
unchanged from the Kubernetes project (Apache-2.0).
