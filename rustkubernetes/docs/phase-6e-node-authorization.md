# Phase 6e — Node authorization

A kubelet's credential now opens only what that node needs, as in upstream Kubernetes. Two pieces work together:

- the **Node authorizer** (`--authorization-mode=Node,RBAC`) decides what a kubelet may *read* and which operations
  it may attempt;
- the **NodeRestriction** admission plugin (`--enable-admission-plugins=NodeRestriction`, which kubeadm sets)
  checks the *content* of its writes.

Before this phase, nodes were authorized by RBAC through a `system:node` ClusterRoleBinding to the `system:nodes`
group, so any kubelet could read every Secret in the cluster.

| Piece | Code | Upstream |
|-------|------|----------|
| Node identity | `plugin/src/auth/nodeidentifier.rs` | `pkg/auth/nodeidentifier` |
| Node authorizer | `plugin/src/auth/node/mod.rs` | `plugin/pkg/auth/authorizer/node/node_authorizer.go` |
| Object graph | `plugin/src/auth/node/graph.rs` | `graph.go`, `graph_populator.go` |
| Static node rules | `bootstrappolicy::node_rules()` | `bootstrappolicy.NodeRules()` |
| NodeRestriction | `plugin/src/admission/noderestriction.rs` | `plugin/pkg/admission/noderestriction` |
| Kubelet labels | `pkg/kubelet/src/apis.rs` | `pkg/kubelet/apis/well_known_labels.go` |
| Admission flags | `--enable-admission-plugins`, `--disable-admission-plugins` | `options.AdmissionOptions` |

## 1. Who is a node

A node is a user named `system:node:<name>` who is also in the group `system:nodes`. kubeadm's `kubelet.conf` and
`kubeadm node-credentials` issue exactly this certificate. A user in the group whose name has no node part gets
`unknown node for user "…"`.

## 2. The Node authorizer

The authorizer keeps a graph of which objects each node may reach. It is built from a storage watch on pods and
PersistentVolumes, as upstream builds it from informers.

| Request | Allowed when |
|---------|--------------|
| secrets, configmaps: get/list/watch | a pod bound to the node references the object (volume, projected source, env, envFrom, imagePullSecrets, CSI secret refs, …). list/watch must name it: `fieldSelector=metadata.name=<name>`. |
| persistentvolumeclaims, persistentvolumes: get | referenced by a pod on the node (a PV through its bound claim); `persistentvolumeclaims/status` update/patch likewise |
| serviceaccounts/token: create | the ServiceAccount of a pod on the node |
| nodes: get/list/watch | only its own Node (by name, or `fieldSelector=metadata.name=<node>`) |
| nodes: create/update/patch, nodes/status: update/patch | allowed; NodeRestriction checks which Node |
| pods: get | only pods bound to the node |
| pods: list/watch | only with `fieldSelector=spec.nodeName=<node>` |
| pods: create/delete, pods/status, pods/eviction | allowed; NodeRestriction checks which pod |
| leases | get/update/patch/delete only its own Lease in `kube-node-lease` (create: NodeRestriction checks the name) |
| csinodes | only its own CSINode |
| anything else | the static `NodeRules()`: e.g. read Services, write Events, TokenReviews and SubjectAccessReviews |

The authorizer never denies. A request it doesn't allow goes on to RBAC, and the 403 carries upstream's reason, for
example:

```
secrets "unrelated" is forbidden: User "system:node:n1" cannot get resource "secrets" in API group "" in the
namespace "e2e": no relationship found between node 'n1' and this object
```

As in Kubernetes since 1.8, the bootstrap `system:node` ClusterRoleBinding exists but has **no subjects**. The
`system:node` ClusterRole carries the same rules as `NodeRules()`.

**Bootstrap reconciliation now behaves as upstream's.** It adds missing rules and subjects to bootstrap roles and
bindings but never removes any. A cluster created before this phase therefore keeps `system:nodes` on that binding,
and its kubelets stay authorized by RBAC as well. That is upstream's upgrade behaviour. Remove the subject by hand to
rely on the Node authorizer alone.

## 3. NodeRestriction

The plugin only acts on requests from nodes.

- **Nodes**
  - A node may create or update only its own Node object.
  - It may not change its taints. It may set them when registering (`--register-with-taints`).
  - It may not set or change `configSource`.
  - It may not set or change labels under `node-restriction.kubernetes.io`, or `kubernetes.io` / `k8s.io` labels
    other than the kubelet's own: hostname, os/arch, zone/region, instance-type, and the `kubelet.kubernetes.io` and
    `node.kubernetes.io` prefixes. So `node-role.kubernetes.io/*` can only be set by an admin.
- **Pods**
  - A node may create only mirror pods (annotation `kubernetes.io/config.mirror`). They must be bound to the node,
    have the node as their single controller owner (with its UID), and reference no Secrets, ConfigMaps, PVCs or
    ServiceAccount.
  - It may delete, update the status of, and evict only pods bound to itself. A status update may not change labels.
- **Tokens:** a node may request ServiceAccount tokens only when they are bound to a pod (with the pod's UID) that
  runs on the node.
- **Leases:** only `kube-node-lease/<node>`.
- **PVC status:** only `status.capacity` and `status.conditions` may change.

Errors use admission's format, for example `nodes "n1" is forbidden: is not allowed to modify labels:
node-role.kubernetes.io/worker`.

Admission now also runs for subresource creates (pod binding, eviction, ServiceAccount token), with the parent object
as the old object, as upstream does. `Priority` and `DefaultTolerationSeconds` skip subresources, as theirs do.

## 4. Flags

- **kube-apiserver**
  - `--authorization-mode=Node,RBAC` now enables the real Node authorizer. It is the default when TLS is on.
  - `--enable-admission-plugins` / `--disable-admission-plugins` choose admission plugins in upstream's fixed order:
    NamespaceLifecycle, ServiceAccount, NodeRestriction, Priority, DefaultTolerationSeconds.
  - NodeRestriction is off by default, as upstream. Unknown names and overlaps are rejected with upstream's messages.
- **kubelet:** `--node-labels` with unknown `kubernetes.io` / `k8s.io` keys stop the kubelet at startup, with
  upstream's message.
- **Request attributes:** list and watch requests now carry their field and label selectors, and
  `fieldSelector=metadata.name=x` names the object for list *and* watch (`RequestInfoFactory`).

`hack/local-up-cluster.sh` (secure mode), both e2e scripts and rustkubeadmin clusters run with `Node,RBAC` and
`NodeRestriction`, like kubeadm.

## 5. Tests

```bash
make test             # graph, authorizer decisions, node identity, label rules, admission flags
make test-e2e-node    # "node authorization": 18 checks run as the kubelet itself (its kubelet.conf)
```

The node e2e suite passes 64/64. Its checks act as the kubelet:
- **Reads:** its own Node yes; all Nodes, another Node, an unrelated Secret, and listing Secrets no; the Secret and
  ConfigMap its pod uses yes.
- **Pods and leases:** listing pods requires `spec.nodeName`; it can read its own Lease, and no Lease elsewhere.
- **Writes (NodeRestriction):** modifying another Node, setting `node-role.kubernetes.io/worker`, changing taints and
  creating non-mirror pods are all refused; `node.kubernetes.io/pool` is allowed.
- **Admin:** may still set the role label.

Every other check in the suite also runs the real kubelet under these rules: volumes, env from Secrets and
ConfigMaps, projected tokens, status, leases, logs and exec. The two-machine network suite passes 36/36 with flannel
and both kubelets, and the kubectl suite passes 39/39.
