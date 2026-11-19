# Roadmap

Each phase ends with something you can **run and test** against the official `kubectl`.
Phases build strictly on the previous ones; the crate layout already reserves their place.

## Phase 1 — Core: API machinery + kube-apiserver ✅

Delivered (see [phase-1-core.md](phase-1-core.md)):

- `k8s-apimachinery`: ObjectMeta/TypeMeta/ListMeta, Status errors, label & field selectors, validation helpers, quantities.
- `k8s-api`: typed `core/v1`, `apps/v1`, `coordination/v1` objects; Kubernetes **protobuf** decoding from upstream `.proto` files.
- `k8s-apiserver`: versioned storage with watch + snapshot persistence, generic REST handlers
  (get/list/watch/create/update/patch/delete/deletecollection, `status`/`scale`/custom subresources), admission chain,
  discovery, server-side Table printing, OpenAPI v3 index, pagination, dry-run, finalizers, graceful deletion.
- `pkg/apis` + `pkg/registry`: defaults, validation and strategies for namespaces, pods, nodes, services (+ ClusterIP /
  NodePort allocation), endpoints, configmaps, secrets, serviceaccounts, events, deployments, replicasets, daemonsets,
  statefulsets, jobs, cronjobs, leases, endpointslices.
- `plugin`: NamespaceLifecycle admission.
- `kube-apiserver` binary, `rkubectl`, `client-go` (REST, typed `Api<T>`, kubeconfig, informers, workqueue).
- Tests: 47 unit/integration tests, `hack/test-cmd.sh` (24 kubectl compatibility checks).

## Phase 2 — kube-controller-manager ✅

Delivered: see [phase-2-controllers.md](phase-2-controllers.md) — 15 controllers, leader election, shared
informers, dynamic informers, events, discovery; API server support for propagation policies and
finalizer-based namespace deletion; 11 controller integration tests.

Original plan:

Binary: `cmd/kube-controller-manager`, library: `pkg/controller/*`.

1. Controller framework: shared informer factory, `ControllerContext`, `--controllers` flag, metrics/healthz.
2. Leader election on a `coordination.k8s.io/v1 Lease` (`client-go/tools/leaderelection`).
3. Controllers, in order:
   - **replicaset** (expectations, adoption via ownerReferences, slow-start batch creation, ranked deletion)
   - **deployment** (RollingUpdate / Recreate, `pod-template-hash`, rollback history, status conditions)
   - **namespace** (finalizer-based content deletion; replaces the Phase 1 in-apiserver cascade)
   - **garbagecollector** (ownerReference graph, Background / Foreground / Orphan propagation)
   - **serviceaccount** (`default` SA per namespace), **root-ca-cert-publisher**
   - **endpoints** + **endpointslice** (needs pod IPs → fully useful after Phase 4)
   - **nodelifecycle** (Lease heartbeats, NotReady/Unreachable taints) + **taint-eviction**
   - **daemonset**, **statefulset**, **job**, **cronjob**, **ttl-after-finished**, **podgc**
4. API server additions: `foregroundDeletion` / `orphan` finalizers, `DefaultTolerationSeconds` admission, `ServiceAccount` admission.
5. Test: `kubectl apply` a Deployment → ReplicaSet and Pods appear; `kubectl rollout status/undo/history` work.

## Phase 3 — kube-scheduler ✅

Delivered: see [phase-3-scheduler.md](phase-3-scheduler.md) — queue, cache, framework, 11 plugins, preemption,
profiles, PriorityClass + Priority admission, and kubemark hollow nodes for end-to-end testing.

Original plan:

Binary: `cmd/kube-scheduler`, library: `pkg/scheduler`.

1. Scheduling queue (activeQ / backoffQ / unschedulable) with priority sort.
2. Scheduler cache + snapshot (`NodeInfo` with requested resources, ports, pod affinity).
3. Framework with extension points: PreFilter, Filter, PostFilter, PreScore, Score, Reserve, Permit, PreBind, Bind.
4. Plugins: NodeName, NodeUnschedulable, NodeResourcesFit (LeastAllocated), NodeAffinity, NodePorts, TaintToleration,
   PodTopologySpread, InterPodAffinity, ImageLocality, DefaultBinder, DefaultPreemption.
5. `KubeSchedulerConfiguration` (profiles, plugin weights).
6. Test: pods get `spec.nodeName`; unschedulable pods show `PodScheduled=False` with reasons; preemption works.

## Phase 4 — kubelet ✅ done

Binary: `cmd/kubelet`, library: `pkg/kubelet` — see [phase-4-kubelet.md](phase-4-kubelet.md).

1. Node registration, status updates (real CPU/memory/disk/OS info), `kube-node-lease` heartbeats.
2. Container runtime: Docker Engine API backend in the style of `cri-dockerd`, behind a CRI-shaped `Runtime`
   trait (containerd's CRI socket needs root, so a containerd backend is deferred to Phase 6).
3. Pod source = API server watch on `spec.nodeName`; per-pod workers; PLEG-style relist every second.
4. Sandboxes, init containers, restart policies with crash-loop backoff, graceful termination, preStop/postStart.
5. Probes (exec/http/tcp, startup/liveness/readiness), container and pod status, pod IPs from the Docker bridge.
6. Volumes: emptyDir, hostPath, configMap, secret, downwardAPI, projected; env vars incl. `$(VAR)` expansion.
7. Kubelet HTTP server for logs / exec; API server `pods/log` (streamed) and `pods/exec` (WebSocket) proxy.
8. Tests: `hack/test-e2e-node.sh` — `kubectl run`, `logs`, `exec`, volumes, probes, Deployments, Jobs, deletion.

## Phase 5 — kube-proxy ✅ done

Binary: `cmd/kube-proxy`, library: `pkg/proxy` — see [phase-5-kube-proxy.md](phase-5-kube-proxy.md).

1. Service / EndpointSlice informers, bounded-frequency sync runner (1 s min, 30 s max).
2. nftables backend: one dedicated table, loaded atomically; ClusterIP, NodePort, externalIPs, LB IPs.
3. Random load balancing, `sessionAffinity: ClientIP`, `internal/externalTrafficPolicy: Local`, terminating-endpoint
   fallback, reject for Services without endpoints, masquerading, hairpin, stale UDP conntrack cleanup.
4. `/healthz`, `/livez`, `/metrics`; `--cleanup`, `--dry-run`; runs in a host-network container with `NET_ADMIN`.
5. Tests: `curl <ClusterIP>` and `curl <node>:<NodePort>` reach pods (`hack/test-e2e-node.sh`).
6. Deferred: iptables/ipvs modes, partial syncs, `loadBalancerSourceRanges`, `healthCheckNodePort`.

## Phase 6 — Production hardening

Split into sub-phases.

### 6a — Security ✅ done · 6b — Cluster DNS ✅ done

See [phase-6-security.md](phase-6-security.md).

- **PKI**: `cmd/kubeadm` (`init phase certs|kubeconfig`, `kubeconfig user`).
- **TLS everywhere**: rustls with the `ring` provider; client-go kubeconfig TLS and in-cluster config.
- **Authentication**: x509, token file, ServiceAccount JWT (ES256), anonymous; impersonation.
- **Authorization**: RBAC API and authorizer, bootstrap policy, escalation prevention, aggregation controller;
  access/token reviews.
- **ServiceAccounts**: TokenRequest, the ServiceAccount admission plugin, projected token volumes,
  root-ca-cert-publisher, OIDC discovery/JWKS.
- **Kubelet**: HTTPS with x509/TokenReview authentication and SubjectAccessReview authorization; the API server
  calls it over TLS with a client certificate.
- **Cluster DNS**: the CoreDNS add-on and kubelet `dnsPolicy`.

### Web console ✅ done

`admin/` — see [admin-console.md](admin-console.md): users, console permissions and RBAC grants (impersonation),
machines with the offline installer (local or SSH), cluster provisioning and lifecycle, and live cluster views.

### 6c — etcd backend ✅ done

See [phase-6c-etcd.md](phase-6c-etcd.md): etcd v3 over gRPC (vendored client generated from etcd's protos), upstream
etcd3 store semantics and key layout, compactor, `etcd` health checks, kubeadm's etcd PKI and flags, encryption at
rest (aesgcm/identity), etcd in local-up, the node e2e suite and console-created clusters.

### 6d — Pod network ✅ done

See [phase-6d-pod-network.md](phase-6d-pod-network.md): the node IPAM controller (RangeAllocator, cidrset),
pod CIDR validation, kubelet NetworkReady and `updatePodCIDR`, kubenet in the dockershim (bridge `cbr0`), the
flannel add-on (vxlan / host-gw, `--ip-masq`), `NetworkUnavailable` taints, and a two-machine e2e suite. rustkubeadmin
clusters use it by default.

### 6e — Node authorization ✅ done

See [phase-6e-node-authorization.md](phase-6e-node-authorization.md): the Node authorizer (object graph,
`NodeRules`, own Node/Lease/pods only), the NodeRestriction admission plugin, `--enable-admission-plugins` /
`--disable-admission-plugins`, kubelet `--node-labels` validation, upstream-style RBAC bootstrap reconciliation.

### 6f — Streaming ✅ done

See [phase-6f-streaming.md](phase-6f-streaming.md): SPDY/3.1 (`spdystream`), kubelet port forwarding into the
pod's network namespace, the API server's WebSocket→SPDY tunnel and SPDY proxy, attach (WebSocket and SPDY),
exec over SPDY, upstream exec/attach parameters, `kubectl cp`.

### 6g — Resource metrics ✅ done

See [phase-6g-metrics.md](phase-6g-metrics.md): kubelet `/metrics/resource`, HTTPS probes, the aggregation layer
(`kube-aggregator`: APIService, autoregistration, availability, discovery, front-proxy proxy), front-proxy PKI and
flags, `extension-apiserver-authentication`, metrics-server v0.8.0 add-on, `kubectl top`.

### 6h — Persistent volumes ✅ done

See [phase-6h-storage.md](phase-6h-storage.md): PV/PVC/StorageClass with upstream validation and printers, the
storage admission plugins, the PV binder (binding, in-tree host-path and external provisioning, reclaim),
protection and ephemeral-volume controllers, scheduler `VolumeBinding`, kubelet PVC mounts, local-path-provisioner
add-on, and a protobuf decoding fix for inlined Go structs.

### 6i — Certificates ✅ done

See [phase-6i-certificates.md](phase-6i-certificates.md): the CSR API and its admission plugins, the signing,
approving and cleaner controllers, bootstrap tokens, kubelet TLS bootstrapping and certificate rotation,
`kubeadm token|join|certs check-expiration|certs renew`.

### 6j — CustomResourceDefinitions ✅ done

See [phase-6j-crds.md](phase-6j-crds.md): `apiextensions.k8s.io/v1` CRDs with upstream validation, naming and
establishing, structural schemas (validation, defaulting, pruning, list types), `fieldValidation` and `Warning`
headers, `status`/`scale` subresources, printer columns, multiple versions with `None`/`Webhook` conversion,
deprecation warnings, OpenAPI v3 for `kubectl explain`, the cleanup finalizer, local APIServices, and garbage
collection of custom resources.

### Next ⏳ (towards a minimal production cluster)

1. 6k — Ingress, NetworkPolicy, ResourceQuota, LimitRange, PodDisruptionBudget, HorizontalPodAutoscaler.
2. 6l — HA control plane (several API servers, multi-member etcd, backup/restore).
3. 6m — audit logging, API Priority and Fairness, control-plane `/metrics`.
4. 6n — containerd CRI runtime; conformance run; OpenAPI v3 schemas for built-in types (`kubectl explain`).
