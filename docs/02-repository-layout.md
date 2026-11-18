# 02 · Repository Layout

## 1. Top level

```
kubernetes/
├── api/            Generated API artifacts: OpenAPI specs, discovery docs, API lint rules
├── build/          Release build system (containerised builds, images, tarballs)
├── CHANGELOG/      Per-minor-release changelogs (1.2 … 1.38)
├── cluster/        Legacy cluster scripts (kube-up/kube-down, GCE, addons manifests)
├── cmd/            main() packages for every binary and helper tool
├── docs/           Generated docs output (man pages, yaml, user-guide); mostly git-ignored
├── hack/           Developer scripts: update-*, verify-*, local-up-cluster, lint config
├── logo/           Kubernetes logo assets and usage guidelines
├── pkg/            Core, non-published implementation code (most of the logic)
├── plugin/         In-tree admission plugins and auth plugins (authorizers, bootstrap token)
├── staging/        Source of the separately published k8s.io/* Go modules
├── test/           e2e, integration, node e2e, conformance, fuzz, test images
├── third_party/    Forked or vendored non-Go dependencies (protobuf, multiarch, gimme)
├── vendor/         Vendored Go dependencies (`go mod vendor`)
├── go.mod / go.sum Module k8s.io/kubernetes
├── go.work         Go workspace that ties the root and all staging modules together
├── Makefile        Symlink to build/root/Makefile
├── .go-version     Pinned Go toolchain (1.27.4)
├── .import-restrictions   Import-boss rules (which packages may import what)
├── OWNERS / OWNERS_ALIASES  Reviewer/approver ownership for Prow
└── AGENTS.md       Guidance for AI coding agents working in this repo
```

## 2. `cmd/`: binaries and tools

Each sub-directory is a `package main`. Real logic lives in `cmd/<x>/app` or in `pkg/`.

| Directory | Kind | Purpose |
|-----------|------|---------|
| `kube-apiserver` | component | API server. `app/server.go` → `NewAPIServerCommand`, `Run`, `CreateServerChain` |
| `kube-controller-manager` | component | Controller manager. `app/controllermanager.go` plus one file per controller group (`apps.go`, `batch.go`, `core.go`…) |
| `kube-scheduler` | component | Scheduler. `app/server.go` → `NewSchedulerCommand`, `Setup`, `Run` |
| `kubelet` | component | Node agent. `app/server.go` → `NewKubeletCommand`, `RunKubelet` |
| `kube-proxy` | component | Service proxy. `app/server.go`, `server_linux.go`, `server_windows.go` |
| `cloud-controller-manager` | component | Reference CCM wiring (`main.go`, `nodeipamcontroller.go`) |
| `kubectl`, `kubectl-convert` | CLI | `kubectl.go` → `kubectl/pkg/cmd.NewDefaultKubectlCommand` |
| `kubeadm` | CLI | `kubeadm.go` → `app.Run()` |
| `gendocs`, `genkubedocs`, `genman`, `genyaml`, `genswaggertypedocs`, `genutils` | generators | Produce CLI reference docs, man pages and YAML for docs |
| `genfeaturegates` | generator | Generates the feature-gate list used by the verify scripts |
| `clicheck` | verifier | Checks kubectl commands follow CLI conventions |
| `fieldnamedocscheck` | verifier | Checks API field docs mention the correct JSON field names |
| `import-boss`, `importverifier`, `preferredimports` | verifiers | Enforce import restrictions and preferred aliases |
| `dependencycheck`, `dependencyverifier` | verifiers | Guard against unwanted dependencies (`hack/unwanted-dependencies.json`) |
| `gotemplate` | utility | Renders Go templates from the command line (used by scripts) |
| `prune-junit-xml` | utility | Shrinks JUnit XML test reports for CI |

## 3. `pkg/`: core implementation

| Package | Purpose | Doc |
|---------|---------|-----|
| `pkg/api` | Helpers over API types (pod/service/PV/PVC field dropping, `legacyscheme`) | [03](03-api-and-type-system.md) |
| `pkg/apis` | **Internal** (hub) API types per group, plus validation, defaults and conversion | [03](03-api-and-type-system.md) |
| `pkg/registry` | REST storage and strategy for every built-in resource | [04](04-kube-apiserver.md) |
| `pkg/controlplane` | Assembles the kube-apiserver instance, its API groups and reconcilers | [04](04-kube-apiserver.md) |
| `pkg/kubeapiserver` | kube-apiserver specific options, authn/authz/admission config | [04](04-kube-apiserver.md) |
| `pkg/admission` | Shared admission helpers | [05](05-admission-plugins.md) |
| `pkg/controller` | Every built-in controller | [06](06-kube-controller-manager.md) |
| `pkg/scheduler` | The scheduler | [07](07-kube-scheduler.md) |
| `pkg/kubelet` | The kubelet | [08](08-kubelet.md) |
| `pkg/proxy` | kube-proxy implementations | [09](09-kube-proxy.md) |
| `pkg/kubectl` | Thin shim; most kubectl code is in staging | [10](10-kubectl.md) |
| `pkg/volume` | Volume plugin framework and in-tree plugins | [13](13-storage-and-volumes.md) |
| `pkg/serviceaccount` | SA token generation and validation (JWT) | [14](14-networking-auth-security.md) |
| `pkg/credentialprovider` | Image pull credential keyrings and plugins | [14](14-networking-auth-security.md) |
| `pkg/auth` | Node identifier and authorizer helpers | [14](14-networking-auth-security.md) |
| `pkg/probe` | exec/http/tcp/grpc probe implementations | [14](14-networking-auth-security.md) |
| `pkg/security`, `pkg/securitycontext` | AppArmor and SecurityContext helpers | [14](14-networking-auth-security.md) |
| `pkg/features` | **All Kubernetes feature gates** (`kube_features.go`) | [16](16-developer-guide.md) |
| `pkg/printers` | Server-side table printing (`kubectl get` columns) | [10](10-kubectl.md) |
| `pkg/quota` | Resource quota evaluators | [06](06-kube-controller-manager.md#resourcequota) |
| `pkg/generated/openapi` | Generated OpenAPI definitions | [03](03-api-and-type-system.md) |
| `pkg/routes` | Extra HTTP routes (`/logs`, OIDC discovery) | [04](04-kube-apiserver.md) |
| `pkg/capabilities` | Global "allow privileged" style capabilities | — |
| `pkg/certauthorization` | Shared authorization checks for certificate signers | [14](14-networking-auth-security.md) |
| `pkg/client` | Client test helpers and wait conditions | — |
| `pkg/cluster/ports` | Well-known component ports (10250 kubelet, 10257 KCM, 10259 scheduler…) | — |
| `pkg/fieldpath` | Resolves `metadata.name`-style field paths (Downward API) | — |
| `pkg/windows/service` | Run components as Windows services | — |
| `pkg/util/*` | Small utilities: `taints`, `tolerations`, `labels`, `hash`, `flock`, `bandwidth`, `goroutinemap`, `kernel`, `oom`, `procfs`, `removeall`, `rlimit`, `tail`… | — |

## 4. `staging/`: published modules

`staging/src/k8s.io/<repo>` is the **source of truth** for modules that are also published as separate repositories, such as `k8s.io/client-go` and `k8s.io/apimachinery`. A publishing bot copies them out after each merge.

- **Do not** edit the published GitHub repos. Edit them here.
- The root `go.work` file and `vendor/k8s.io/*` symlinks make `import "k8s.io/client-go/..."` resolve to `staging/`.
- `pkg/` may import staging, but **staging may never import `k8s.io/kubernetes/pkg/...`**. This is enforced by `hack/verify-imports.sh` and `.import-restrictions`.

Full module list and descriptions: [12 · Staging libraries](12-staging-libraries.md).

## 5. `plugin/`

```
plugin/pkg/
├── admission/   ~27 in-tree admission plugins (see doc 05)
└── auth/
    ├── authenticator/token/bootstrap   Bootstrap-token authenticator (system:bootstrappers)
    └── authorizer/
        ├── node    Node authorizer (graph-based: kubelets can only read their own pods' secrets…)
        └── rbac    RBAC authorizer + bootstrap policy (default ClusterRoles/Bindings)
```

## 6. `test/`

| Directory | Contents |
|-----------|----------|
| `e2e/` | Ginkgo end-to-end tests organised by SIG (`apps`, `network`, `storage`, `node`, `scheduling`, `auth`, `dra`…) plus `framework/` |
| `e2e_node/` | Node e2e: runs the kubelet against a real runtime on one machine |
| `e2e_node_windows/`, `e2e_kubeadm/`, `e2e_dra/` | Specialised e2e suites |
| `integration/` | Integration tests that start a real apiserver and etcd in-process, with no kubelets |
| `conformance/` | Conformance test metadata (`testdata/conformance.yaml`) and tooling |
| `fuzz/` | Fuzz targets |
| `images/` | Source for the test container images (`agnhost`, `busybox`, …) |
| `utils/` | Shared test helpers |
| `soak/`, `list/`, `typecheck/`, `compatibility_lifecycle/`, `declarative_validation/` | Misc tooling and suites |

## 7. Generated code: how to recognise it

| Filename pattern | Generator | Contents |
|------------------|-----------|----------|
| `zz_generated.deepcopy.go` | deepcopy-gen | `DeepCopy`, `DeepCopyInto`, `DeepCopyObject` |
| `zz_generated.conversion.go` | conversion-gen | versioned ↔ internal conversions |
| `zz_generated.defaults.go` | defaulter-gen | `SetObjectDefaults_*` |
| `zz_generated.validations.go` | validation-gen | Declarative validation from `+k8s:` tags |
| `zz_generated.model_name.go` | openapi model name gen | OpenAPI model names |
| `generated.pb.go`, `generated.proto` | go-to-protobuf | Protobuf serialisation |
| `types_swagger_doc_generated.go` | genswaggertypedocs | Field docs |
| `staging/.../client-go/{kubernetes,informers,listers,applyconfigurations}` | client-gen, informer-gen, lister-gen, applyconfiguration-gen | Typed clients |

**Never hand-edit generated files.** Regenerate them with `hack/update-codegen.sh`.
