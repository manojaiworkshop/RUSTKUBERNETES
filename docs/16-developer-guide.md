# 16 · Developer Guide: Common Change Recipes

This guide shows **which files to touch** for the most common kinds of contribution. Also read the repo's own [`AGENTS.md`](../kubernetes/AGENTS.md) and [`CONTRIBUTING.md`](../kubernetes/CONTRIBUTING.md).

## 0. Ground rules (from `AGENTS.md`)

- **Generated files are read-only.** Never hand-edit `zz_generated.*` or `generated.pb.go`. Run `make update` (or the specific `hack/update-*.sh`).
- **`go.mod` / `go.work` are generated.** Use `hack/pin-dependency.sh` and `hack/update-vendor.sh`. Never run `go mod tidy` by hand.
- **Staging is the source of truth** for `k8s.io/*`. Staging must not import `k8s.io/kubernetes`.
- **Every `.go` file needs the license header** from `hack/boilerplate/boilerplate.go.txt`.
- Most behaviour changes require a **KEP** (kubernetes/enhancements) and a **feature gate**.
- Use **contextual logging**: `logger := klog.FromContext(ctx)` and structured key/value pairs (`logger.Info("msg", "pod", klog.KObj(pod))`). Enforced by `hack/logcheck.conf`.

## 1. Adding a new field to a built-in API (for example `PodSpec.Foo`)

| Step | File(s) |
|------|---------|
| 1. Add the feature gate | `pkg/features/kube_features.go` (constant + versioned spec) |
| 2. External type with JSON/protobuf tags, docs and `+optional`, `+featureGate=Foo`, `+k8s:` validation tags | `staging/src/k8s.io/api/core/v1/types.go` |
| 3. Internal type | `pkg/apis/core/types.go` |
| 4. Defaults (if any) | `pkg/apis/core/v1/defaults.go` |
| 5. Validation | `pkg/apis/core/validation/validation.go` (+ declarative tags) |
| 6. **Drop the field when the gate is disabled** (unless already in use in the old object) | `pkg/api/pod/util.go` → `dropDisabledFields` |
| 7. Strategy changes if needed | `pkg/registry/core/pod/strategy.go` |
| 8. Regenerate | `hack/update-codegen.sh`, `hack/update-openapi-spec.sh` (produces deepcopy, conversion, protobuf, openapi, applyconfigurations, `api/openapi-spec`) |
| 9. Consumers | kubelet, scheduler, controllers: guard with `utilfeature.DefaultFeatureGate.Enabled(features.Foo)` |
| 10. Tests | Validation unit tests, `pkg/api/pod/util_test.go` drop tests, round-trip fuzzers (`pkg/apis/core/fuzzer`), integration and e2e tests with `framework.WithFeatureGate` |

Fields must be **optional** (or have defaults) for backward compatibility. Never rename or remove fields in a GA version.

## 2. Adding a new API resource / group version

1. Types: `staging/src/k8s.io/api/<group>/<version>/{types.go,register.go,doc.go}` with `+genclient`, `+k8s:prerelease-lifecycle-gen:introduced=1.x`.
2. Internal: `pkg/apis/<group>/{types.go,register.go}`, `install/install.go`, `<version>/{defaults.go,register.go,doc.go}`, `validation/`.
3. Registry: `pkg/registry/<group>/<resource>/{strategy.go,storage/storage.go}` and `pkg/registry/<group>/rest/storage_<group>.go` (`RESTStorageProvider`).
4. Wire into `pkg/controlplane/instance.go` → `StorageProviders` and default enablement (`DefaultAPIResourceConfigSource`; beta APIs are off by default).
5. Storage settings: `pkg/kubeapiserver/default_storage_factory_builder.go`.
6. RBAC bootstrap roles if components need access: `plugin/pkg/auth/authorizer/rbac/bootstrappolicy`.
7. kubectl printing columns: `pkg/printers/internalversion/printers.go`.
8. Node authorizer graph if kubelets need it: `plugin/pkg/auth/authorizer/node`.
9. Run `make update`. API reviewers look at `api/openapi-spec` and `api/api-rules` diffs.

## 3. Adding a new controller to kube-controller-manager

1. Implement it in `pkg/controller/<name>/` following the [canonical pattern](06-kube-controller-manager.md#3-the-canonical-controller-pattern) (informers → workqueue → `sync(key)`).
2. Add the name constant in `cmd/kube-controller-manager/names/controller_names.go`.
3. Add the descriptor and constructor in the right `cmd/kube-controller-manager/app/<area>.go`:
   ```go
   func newFooControllerDescriptor() *ControllerDescriptor {
       return &ControllerDescriptor{
           name:                 names.FooController,
           constructor:          newFooController,
           requiredFeatureGates: []featuregate.Feature{features.Foo},
       }
   }
   ```
4. Register it in `NewControllerDescriptors` (`controller_descriptor.go`).
5. RBAC: add a controller role in `plugin/pkg/auth/authorizer/rbac/bootstrappolicy/controller_policy.go` (plus testdata `controller-roles.yaml` / `controller-role-bindings.yaml`).
6. Config or flags (if needed): `staging/src/k8s.io/kube-controller-manager/config/v1alpha1` and `cmd/kube-controller-manager/app/options`.
7. Tests: unit tests with fake clientsets and informers, and integration tests under `test/integration/<name>`.

## 4. Adding a scheduler plugin

1. `pkg/scheduler/framework/plugins/<name>/<name>.go`: implement the extension-point interfaces from `k8s.io/kube-scheduler/framework` plus `EventsToRegister` (queueing hints).
2. Name constant in `plugins/names/names.go`. Register in `plugins/registry.go` (`NewInTreeRegistry`).
3. Enable by default (if appropriate) in `pkg/scheduler/apis/config/v1/default_plugins.go`. Plugin args go in `pkg/scheduler/apis/config/types_pluginargs.go` plus the v1 versions and validation.
4. Feature flags: add to `plugins/feature/feature.go` and wire in `registry.go`.
5. Tests: unit tests, `pkg/scheduler/testing` wrappers (`st.MakePod()…`), integration tests in `test/integration/scheduler`, and benchmarks in `scheduler_perf`.

## 5. Adding an admission plugin

See [05 · §4](05-admission-plugins.md#4-writing-an-admission-plugin-pattern). Register it in `pkg/kubeapiserver/options/plugins.go` (both the ordered list and registration) and decide whether it is on by default (`DefaultOffAdmissionPlugins`).

## 6. Adding a kubectl command or flag

1. `staging/src/k8s.io/kubectl/pkg/cmd/<verb>/`, using the Options pattern (Complete / Validate / Run).
2. Register it in `kubectl/pkg/cmd/cmd.go` under the right group.
3. i18n: wrap user text in `i18n.T(...)`, plus `templates.LongDesc` / `templates.Examples`.
4. Tests: unit tests with `cmdtesting.NewTestFactory()`, and `test/cmd/*.sh`.
5. `hack/verify-cli-conventions.sh` must pass.

## 7. Adding a kubelet feature

- Config: `staging/src/k8s.io/kubelet/config/v1beta1/types.go` → internal `pkg/kubelet/apis/config/types.go`, defaults, validation (`pkg/kubelet/apis/config/validation`).
- Runtime behaviour: `pkg/kubelet/...` with a gate. If the runtime is involved, extend CRI (`staging/src/k8s.io/cri-api/pkg/apis/runtime/v1/api.proto`) in coordination with containerd and CRI-O.
- Tests: unit tests, `test/e2e_node` (with `[NodeFeature:Foo]` / feature labels).

## 8. Debugging tips

| Goal | How |
|------|-----|
| More logs | `-v=4` (kubelet / KCM / scheduler); `--vmodule=kubelet*=6`; JSON logs with `--logging-format=json` |
| Debug build | `make WHAT=cmd/kubelet DBG=1`, then `dlv exec _output/bin/kubelet -- <flags>` |
| Profiling | `/debug/pprof` on component secure ports (`--profiling`); `hack/grab-profiles.sh` |
| apiserver audit | `--audit-policy-file`, `--audit-log-path` |
| Scheduler cache dump | `kill -USR2 <scheduler-pid>` |
| Inspect etcd | `etcdctl get /registry/pods/default/foo --prefix` (values are protobuf; use `auger` to decode) |
| Raw API calls | `kubectl get --raw /api/v1/nodes/<n>/proxy/stats/summary`, `kubectl get --raw /metrics` |
| Why is my pod Pending? | `kubectl describe pod` (scheduler events), scheduler logs at `-v=5`, `kubectl get events --for pod/<name>` |
| Why does my controller not act? | Check leader election (`kubectl -n kube-system get lease`), `workqueue_*` metrics, RBAC denials in apiserver audit |

## 9. Useful entry points to start reading

| You want to understand… | Start at |
|-------------------------|----------|
| How any request is processed | `staging/src/k8s.io/apiserver/pkg/server/config.go` → `DefaultBuildHandlerChain` |
| How a pod is scheduled | `pkg/scheduler/schedule_one.go` → `ScheduleOne` |
| How a pod is started | `pkg/kubelet/kubelet.go` → `SyncPod` → `pkg/kubelet/kuberuntime/kuberuntime_manager.go` → `SyncPod` |
| How a rolling update works | `pkg/controller/deployment/rolling.go` → `rolloutRolling` |
| How a Service gets traffic | `pkg/controller/endpointslice` → `pkg/proxy/nftables/proxier.go` → `syncProxyRules` |
| How watch works | `staging/src/k8s.io/client-go/tools/cache/reflector.go` and `apiserver/pkg/storage/cacher/cacher.go` |
| How deletion and GC work | `apiserver/pkg/registry/generic/registry/store.go` → `Delete`, and `pkg/controller/garbagecollector/garbagecollector.go` |
