# Phase 2 — kube-controller-manager

Phase 2 adds the `kube-controller-manager` binary and the `pkg/controller` crate with 15 controllers, plus the
API server and client-go features they need.

## 1. Architecture

```mermaid
flowchart LR
  API[(kube-apiserver)] -- LIST+WATCH --> F[SharedInformerFactory<br/>one informer per type]
  F --> S1[Store<Pod>] & S2[Store<ReplicaSet>] & S3[...]
  F -- events --> H[event handlers<br/>enqueue keys]
  H --> Q[WorkQueue<br/>dedup + rate limit]
  Q --> W[workers → sync(key)]
  W -- read --> S1
  W -- create/update/delete --> API
```

| Piece | Code |
|-------|------|
| Binary, flags, `--controllers`, leader election, `/healthz` | `cmd/kube-controller-manager/src/app/` |
| Controller registry (`ControllerDescriptor`, names/aliases) | `pkg/controller/src/lib.rs` |
| Worker loop, expectations, adoption, pod helpers, hashing | `pkg/controller/src/controller_utils.rs` |
| Shared informers | `staging/.../client-go/src/informers.rs` |
| Typed + dynamic informers (`ListerWatcher`) | `client-go/src/tools/cache.rs` |
| Lease leader election | `client-go/src/tools/leaderelection.rs` |
| Events | `client-go/src/tools/record.rs` |
| Discovery | `client-go/src/discovery.rs` |

**Startup** (`start_controllers`): every enabled controller is constructed first (subscribing to informer events),
then the factory starts the watches, waits for cache sync, and every controller's `run` starts its workers plus a
periodic resync (`--min-resync-period`).

**Worker semantics** (`run_workers`): `Ok(None)` → done; `Ok(Some(d))` → requeue after `d`;
conflict → quiet rate-limited retry; "namespace is being terminated" → drop; other errors → rate-limited
retry (5 ms · 2ⁿ, max 15 retries).

## 2. Controllers

| `--controllers` name (alias) | What it does | Notable behaviour |
|------------------------------|--------------|-------------------|
| `replicaset-controller` (`replicaset`) | keeps `replicas` pods | expectations, adoption/release, slow-start batches, ranked deletion, status with ready/available |
| `deployment-controller` (`deployment`) | Deployment → ReplicaSets | RollingUpdate (maxSurge/maxUnavailable, unhealthy-first scale-down), Recreate, `pod-template-hash`, revisions + `rollout history/undo`, scaling events, history cleanup, `Available`/`Progressing` conditions incl. `ProgressDeadlineExceeded` |
| `statefulset-controller` (`statefulset`) | ordered pods `name-0..N-1` | OrderedReady / Parallel, stable hostname/subdomain, highest-ordinal-first scale-down, RollingUpdate with partition |
| `daemonset-controller` (`daemonset`) | one pod per eligible node | nodeSelector + taints/tolerations, daemon default tolerations, node pinning via `metadata.name` affinity, rolling update |
| `job-controller` (`job`) | runs pods to completion | parallelism/completions, work-queue jobs, Indexed mode (`JOB_COMPLETION_INDEX`), backoffLimit, activeDeadlineSeconds, suspend/resume, `Complete`/`Failed`/`SuccessCriteriaMet` conditions |
| `cronjob-controller` (`cronjob`) | Jobs on a cron schedule | own 5-field cron parser (+ macros), startingDeadlineSeconds, Allow/Forbid/Replace, history limits, requeue at next run |
| `ttl-after-finished-controller` | deletes finished Jobs after `ttlSecondsAfterFinished` | uid precondition |
| `namespace-controller` (`namespace`) | empties terminating namespaces | discovery + `deletecollection` of every namespaced resource, then `/finalize` |
| `garbage-collector-controller` (`garbagecollector`) | ownerReference cascading | dynamic informers on every resource, owner graph, Background / Foreground (`foregroundDeletion`) / Orphan (`orphan`), API double-check before deleting |
| `serviceaccount-controller` | `default` SA in every namespace | |
| `endpoints-controller` (`endpoint`) | Service + Pods → Endpoints | named targetPort resolution per pod, ready/notReady addresses, `publishNotReadyAddresses` |
| `endpointslice-controller` | Service + Pods → EndpointSlices | one slice per port set, ready/serving/terminating conditions, owned by the Service |
| `node-lifecycle-controller` (`nodelifecycle`) | node health | Lease / heartbeat timeout → conditions `Unknown`, pods marked not ready, `not-ready`/`unreachable`/`unschedulable` taints |
| `taint-eviction-controller` | NoExecute eviction | honours `tolerationSeconds` from `taint.timeAdded` |
| `pod-garbage-collector-controller` (`podgc`) | pod cleanup | terminated-pod threshold, orphaned pods (node gone), unscheduled terminating pods |

## 3. API server changes in Phase 2

| Change | Where |
|--------|-------|
| `propagationPolicy` (`Foreground` → `foregroundDeletion` finalizer, `Orphan` → `orphan` finalizer; `orphanDependents` legacy field) | `apiserver/src/handlers.rs` (`delete_one`) |
| Namespace deletion is finalizer-based: phase `Terminating` until `spec.finalizers` is emptied through `namespaces/<n>/finalize` | `Strategy::can_remove` / `on_delete_marked`, `pkg/registry/src/core/namespace.rs` |
| Job defaults + generated selector (`batch.kubernetes.io/controller-uid`), Job/CronJob validation | `pkg/apis/src/batch/`, `pkg/registry/src/batch.rs` |
| `DefaultTolerationSeconds` admission (300 s `not-ready`/`unreachable` tolerations) | `plugin/src/admission/defaulttolerationseconds.rs` |
| Tables for jobs, cronjobs, endpointslices | `pkg/registry/src/batch.rs` |
| Lossless pod templates (unmodelled `PodSpec`/`Container` fields kept via flattened maps) | `k8s-api/src/core/v1.rs` |

## 4. Running it

```bash
hack/local-up-cluster.sh          # starts kube-apiserver + kube-controller-manager
export KUBECONFIG=$PWD/_output/local/kubeconfig
kubectl create deploy web --image=nginx --replicas=3
kubectl get deploy,rs,pods
kubectl get events
kubectl rollout history deploy/web
kubectl delete deploy web          # RS and pods are garbage collected
```

Pods stay `Pending` until the scheduler (Phase 3) and kubelet (Phase 4) exist, so `kubectl rollout status`
waits and rolling updates pause once `maxUnavailable` is exhausted — exactly what real Kubernetes does with
pods that never become available. The integration tests use a simulated kubelet
(`test/integration/src/lib.rs::simulate_kubelet`) to drive full rollouts.

## 5. Tests

* `test/integration/tests/controller_test.rs` — 11 end-to-end tests (ReplicaSet, Deployment rolling/recreate,
  GC policies, namespace deletion + default SA, Endpoints/EndpointSlices, Job success/failure, StatefulSet,
  DaemonSet, node lifecycle + taint eviction, leader election).
* `pkg/controller` unit tests: cron schedules, rollout math, hashing, expectations, `--controllers` flags.
* `hack/test-cmd.sh`: 9 controller checks with the official kubectl against the real binaries.

## 6. Known simplifications

| Area | Phase 2 | Upstream |
|------|---------|----------|
| Deployment proportional scaling during a rollout | difference goes to the new RS | proportional across RSs |
| Job failure backoff | failed pods replaced immediately | exponential backoff 10 s·2ⁿ |
| Job pod tracking | counted from pod objects | tracking finalizers + `uncountedTerminatedPods` |
| StatefulSet | no `volumeClaimTemplates`, no ControllerRevision objects | PVCs + revisions |
| DaemonSet node eligibility | nodeSelector + taints | also full node affinity |
| CronJob time zones | UTC only | IANA zones |
| Node lifecycle | no zone-based eviction rate limiting | rate-limited per zone |
| GC | resources discovered once at startup | periodic rediscovery |
