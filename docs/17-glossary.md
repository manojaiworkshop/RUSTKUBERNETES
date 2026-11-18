# 17 · Glossary

| Term | Meaning (and where it lives in code) |
|------|---------------------------------------|
| **Admission** | Plugins that mutate or validate requests after authz and before persistence (`apiserver/pkg/admission`, `plugin/pkg/admission`) |
| **Aggregation layer** | kube-aggregator proxying `APIService` groups to extension apiservers |
| **ApplySet** | Labels and annotations grouping objects applied together, for `kubectl apply --prune` |
| **Assume (scheduler)** | Optimistically adding a pod to the scheduler cache before binding completes |
| **Binding** | `pods/binding` subresource that sets `spec.nodeName` |
| **Bookmark** | Watch event carrying only a `resourceVersion`, so clients can resume watches cheaply |
| **CEL** | Common Expression Language. Used in VAP/MAP, CRD validation, DRA selectors, authn/authz config, match conditions |
| **Cacher / watch cache** | apiserver in-memory cache of a resource, fed by one etcd watch (`storage/cacher`) |
| **Cgroup** | Linux control group. The kubelet manages `kubepods` → QoS → pod → container hierarchy |
| **ClusterIP** | Virtual Service IP implemented by kube-proxy |
| **Compatibility version / emulated version** | A binary behaving like an older minor version for safe upgrades (`component-base/compatibility`) |
| **Conformance** | Tests every certified Kubernetes distribution must pass |
| **Controller** | Reconcile loop over API objects (`pkg/controller`) |
| **ControllerRevision** | Snapshot of a StatefulSet or DaemonSet template, used for rollouts and rollback |
| **CRI** | Container Runtime Interface (`cri-api`). gRPC between the kubelet and containerd / CRI-O |
| **CSI** | Container Storage Interface. Out-of-tree storage drivers (`pkg/volume/csi`) |
| **CycleState** | Per-scheduling-cycle plugin data store |
| **DeltaFIFO** | client-go queue of object deltas feeding informers |
| **Device plugin** | gRPC plugin advertising extended resources (`nvidia.com/gpu`) to the kubelet |
| **DRA** | Dynamic Resource Allocation: `ResourceClaim`, `DeviceClass`, `ResourceSlice` (`resource.k8s.io`) |
| **Emulated version** | See compatibility version |
| **EndpointSlice** | Scalable list of Service backends (`discovery.k8s.io/v1`) |
| **Ephemeral container** | Debug container added to a running pod (`kubectl debug`) |
| **Eviction (API)** | `pods/eviction` subresource that respects PDBs. The newer declarative `EvictionRequest`/`Eviction` live in `lifecycle.k8s.io` |
| **Eviction (kubelet)** | Node-pressure eviction (`pkg/kubelet/eviction`) |
| **Expectations** | Controller bookkeeping of in-flight creates and deletes to avoid stale-cache double actions |
| **Feature gate** | Named on/off switch for in-development features (`pkg/features`) |
| **Field manager** | Identity owning fields under server-side apply (`metadata.managedFields`) |
| **Finalizer** | String in `metadata.finalizers` that blocks deletion until a controller removes it |
| **Gang scheduling** | All-or-nothing scheduling of a PodGroup (`gangscheduling` plugin) |
| **Generation / observedGeneration** | `metadata.generation` increments on spec change. Controllers report `status.observedGeneration` |
| **GVK / GVR** | GroupVersionKind (type) / GroupVersionResource (URL path) |
| **Hub (internal) version** | Internal type every external version converts through (`pkg/apis/<group>`) |
| **Informer** | Cached, watch-driven view of a resource with event handlers (client-go) |
| **KEP** | Kubernetes Enhancement Proposal |
| **Lease** | `coordination.k8s.io` object used for leader election and node heartbeats |
| **Lister** | Read-only, indexer-backed accessor of an informer's cache |
| **Mirror pod** | API representation of a static pod |
| **NodeInfo** | Scheduler's aggregated view of a node (requested resources, pods, ports, images) |
| **Nominated node** | `status.nominatedNodeName`, set by preemption |
| **Opportunistic batching** | Scheduler reuse of filter and score results for consecutive identical pods |
| **OwnerReference** | Link from a dependent to its owner. Drives GC and adoption |
| **PDB** | PodDisruptionBudget, which limits voluntary disruptions |
| **Placement** | Candidate node subset for scheduling a pod group |
| **PLEG** | Pod Lifecycle Event Generator in the kubelet |
| **Pod sandbox** | Runtime environment (network namespace, pause container / VM) holding a pod's containers |
| **PodGroup / Workload** | Scheduling API grouping pods for gang and topology-aware scheduling (`scheduling.k8s.io`) |
| **Preemption** | Evicting lower-priority pods so a higher-priority pod fits |
| **Priority & Fairness (APF)** | apiserver request queuing by `FlowSchema` / `PriorityLevelConfiguration` |
| **Projected volume** | Volume combining SA tokens, secrets, configmaps, downward API, ClusterTrustBundles and pod certificates |
| **QoS class** | Guaranteed / Burstable / BestEffort |
| **Queueing hint** | Scheduler plugin callback deciding whether a cluster event may make a rejected pod schedulable |
| **Reconcile** | Driving observed state toward desired state |
| **Reflector** | client-go component doing LIST+WATCH into a store |
| **resourceVersion** | Opaque object version (etcd revision) used for optimistic concurrency and watch resumption |
| **RESTMapper** | GVK ↔ GVR mapping from discovery |
| **Scheme** | Registry of Go types ↔ GVKs, plus conversions and defaults |
| **Server-side apply (SSA)** | Declarative PATCH with field ownership and conflict detection |
| **Sharding (list/watch)** | Hash-range selectors splitting list/watch across consumers (`apimachinery/pkg/sharding`) |
| **Sidecar container** | Init container with `restartPolicy: Always` that runs alongside the app containers |
| **Staging** | `staging/src/k8s.io/*`, the source of separately published modules |
| **Static pod** | Pod defined by a file on the node, run by the kubelet without the scheduler |
| **Storage version** | The API version objects are encoded with in etcd |
| **Strategy** | Per-resource create/update/delete hooks in the registry |
| **Subresource** | Sub-path of a resource: `status`, `scale`, `binding`, `eviction`, `log`, `exec`, `resize`… |
| **Taint / Toleration** | Node repulsion and pod exception mechanism (`NoSchedule`, `PreferNoSchedule`, `NoExecute`) |
| **Topology Manager** | Kubelet component aligning CPU, memory and device NUMA placement |
| **VAP / MAP** | ValidatingAdmissionPolicy / MutatingAdmissionPolicy (CEL-based admission) |
| **Watch** | Long-running stream of ADDED / MODIFIED / DELETED / BOOKMARK events |
| **Workqueue** | Rate-limited, de-duplicating key queue for controllers |
