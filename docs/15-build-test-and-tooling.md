# 15 · Build, Test & Tooling

## 1. Prerequisites

- Go version pinned in `.go-version` (**1.27.4**). `hack/lib/golang.sh` checks or downloads it (`GOTOOLCHAIN`).
- etcd for integration tests: `./hack/install-etcd.sh` (installs into `third_party/etcd`, which must be on `PATH`).
- Docker or Podman only for containerised release builds (`build/`).
- About 16 GB RAM is recommended for full builds and verify runs.

## 2. Makefile (`Makefile` → `build/root/Makefile`)

Targets delegate to `hack/make-rules/*.sh`:

| Command | Script | What it does |
|---------|--------|--------------|
| `make` / `make all WHAT=cmd/kubelet` | `make-rules/build.sh` | Builds binaries into `_output/bin` (`_output/local/bin/<os>/<arch>`). `WHAT` limits the build to specific packages. `GOFLAGS`, `GOGCFLAGS=-N -l` (debug) and `DBG=1` are supported |
| `make test WHAT=./pkg/kubelet/... GOFLAGS=-v KUBE_TEST_ARGS='-run TestX'` | `make-rules/test.sh` | Unit tests (`go test`) |
| `make test-integration WHAT=./test/integration/scheduler` | `make-rules/test-integration.sh` | Integration tests (starts etcd) |
| `make test-cmd` | `make-rules/test-cmd.sh` | kubectl command-line tests (`test/cmd`) |
| `make test-e2e-node` | `make-rules/test-e2e-node.sh` | Node e2e (local or remote GCE) |
| `make verify` / `make quick-verify` | `make-rules/verify.sh` | Runs every `hack/verify-*.sh` |
| `make update` | `make-rules/update.sh` | Runs every `hack/update-*.sh` (codegen, docs, gofmt, vendor…) |
| `make cross` | `make-rules/cross.sh` | Builds for all platforms |
| `make release` / `quick-release` / `release-images` | `build/release.sh` | Containerised release builds |
| `make clean` | `make-rules/clean.sh` | Removes `_output` |
| `make help` | `make-rules/make-help.sh` | |
| `make ginkgo` | | Builds the ginkgo binary |
| `make lint` | | golangci-lint |

## 3. `build/`: release builds

| File | Purpose |
|------|---------|
| `build/run.sh` | Runs a command inside the `kube-build` container (`build/build-image/`) |
| `build/release.sh`, `release-in-a-container.sh` | Full release: cross-builds, tests, tarballs, images |
| `build/package-tarballs.sh` | `kubernetes-server-linux-amd64.tar.gz`, client and node tarballs |
| `build/release-images.sh`, `build/server-image/` | Component container images (distroless based) |
| `build/pause/` | The **pause** (sandbox) container image source |
| `build/dependencies.yaml` | Central list of dependency versions (Go, etcd, CoreDNS, …) checked by `hack/verify-external-dependencies-version.sh` |
| `build/lib/` | Release helper libraries |
| `build/tools.go` | Pins tool dependencies |
| `build/common.sh`, `util.sh` | Shared shell helpers |

## 4. `hack/`: developer scripts

### 4.1 Libraries (`hack/lib`)
`init.sh` (sourced by every script), `golang.sh` (build env and target lists `KUBE_SERVER_TARGETS`, `KUBE_CLIENT_TARGETS`, `KUBE_NODE_TARGETS`), `etcd.sh`, `test.sh`, `util.sh`, `version.sh` (ldflags version stamping), `logging.sh`, `protoc.sh`, `verify-generated.sh`.

### 4.2 Running a local cluster
- **`hack/local-up-cluster.sh`** builds and runs etcd, apiserver, KCM, scheduler, kubelet and kube-proxy as local processes. This is the fastest end-to-end dev loop.
- `hack/dev-build-and-up.sh`, `dev-build-and-push.sh`, `dev-push-conformance.sh` support remote dev clusters.
- `cluster/kube-up.sh` / `kube-down.sh` are the legacy GCE scripts (`cluster/gce`).

### 4.3 Update scripts (generate or fix)

| Script | Generates |
|--------|-----------|
| `update-codegen.sh` | deepcopy, defaults, conversion, validation, clients, listers, informers, applyconfigs, protobuf, openapi |
| `update-openapi-spec.sh` | `api/openapi-spec`, `api/discovery` (starts an apiserver) |
| `update-generated-docs.sh`, `generate-docs.sh` | CLI reference docs, man pages |
| `update-vendor.sh`, `pin-dependency.sh` | `go.mod`, `go.sum` and `vendor/` across all modules |
| `update-gofmt.sh`, `update-import-aliases.sh` | Formatting and import aliases |
| `update-featuregates.sh` | Feature gate lists |
| `update-mocks.sh` | mockery mocks |
| `update-conformance-yaml.sh` | `test/conformance/testdata/conformance.yaml` |
| `update-golangci-lint-config.sh` | `hack/golangci*.yaml` from `golangci.yaml.in` |
| `update-generated-stable-metrics.sh`, `update-metrics-documentation-list.sh` | Stable metrics lists |
| `update-translations.sh` | kubectl i18n |
| `update-owners-fmt.sh`, `update-vendor-licenses.sh`, `update-internal-modules.sh`, `update-netparse-cve.sh`, `update-kustomize.sh`, `update-go-apidocs.sh`, `update-generated-api-compatibility-data.sh` | Other generated content |

### 4.4 Verify scripts (CI gates, `hack/verify-*.sh`)

| Category | Scripts |
|----------|---------|
| Codegen in sync | `verify-codegen.sh`, `verify-openapi-spec.sh`, `verify-generated-docs.sh`, `verify-mocks.sh`, `verify-conformance-yaml.sh`, `verify-featuregates.sh` |
| Style and lint | `verify-gofmt.sh`, `verify-golangci-lint.sh` (+ `-pr-hints`, `-config`), `verify-shellcheck.sh`, `verify-spelling.sh`, `verify-boilerplate.sh` (license headers via `hack/boilerplate`), `verify-description.sh`, `verify-fieldname-docs.sh`, `verify-cli-conventions.sh`, `verify-pkg-names.sh` |
| Imports and deps | `verify-imports.sh`, `verify-import-boss.sh`, `verify-import-aliases.sh`, `verify-no-vendor-cycles.sh`, `verify-vendor.sh`, `verify-vendor-licenses.sh`, `verify-licenses.sh`, `verify-external-dependencies-version.sh`, `verify-govulncheck.sh`, `verify-internal-modules.sh`, `verify-publishing-bot.sh`, `verify-staging-meta-files.sh`, `verify-readonly-packages.sh`, `verify-prometheus-imports.sh`, `verify-testing-import.sh` |
| API rules | `verify-api-groups.sh`, `verify-prerelease-lifecycle-tags.sh`, `verify-non-mutating-validation.sh`, `apidiff.sh` |
| Metrics | `verify-generated-stable-metrics.sh`, `verify-metrics-naming.sh`, `verify-metrics-documentation-list.sh` |
| Tests | `verify-test-code.sh`, `verify-test-featuregates.sh`, `verify-e2e-test-ownership.sh`, `verify-e2e-images.sh`, `verify-test-images.sh`, `verify-conformance-requirements.sh` |
| Other | `verify-typecheck.sh` (cross-platform compile), `verify-file-sizes.sh`, `verify-owners-fmt.sh`, `verify-netparse-cve.sh`, `verify-deadcode-elimination.sh` |

`hack/verify-all.sh` runs them all. Linter config lives in `hack/golangci.yaml`, `logcheck.conf` (structured logging) and `kube-api-linter/`.

### 4.5 Misc scripts
`hack/ginkgo-e2e.sh` (run e2e against a cluster), `e2e-node-test.sh`, `benchmark-go.sh`, `grab-profiles.sh`, `cherry_pick_pull.sh`, `module-graph.sh`, `lint-dependencies.sh`, `run-prometheus-on-etcd-scrapes.sh`, `print-workspace-status.sh`.

## 5. Test suites

| Suite | Location | How it runs | When to use it |
|-------|----------|-------------|----------------|
| **Unit** | `*_test.go` next to the code | `make test` / `go test ./pkg/...` | Pure logic, fake clients (`client-go/kubernetes/fake`), `ktesting` |
| **Integration** | `test/integration/<area>` | Real apiserver + etcd in-process (`test/integration/framework`, `cmd/kube-apiserver/app/testing.StartTestServer`), controllers started manually, no kubelet | API behaviour, controller + apiserver interaction |
| **e2e** | `test/e2e/<sig>` | Ginkgo against a real cluster (`hack/ginkgo-e2e.sh`, kind, kubetest2) | Full system behaviour |
| **Conformance** | `framework.ConformanceIt(...)` tests in `test/e2e` | Same as e2e with `--ginkgo.focus=\[Conformance\]` | Certified Kubernetes behaviour. Metadata in `test/conformance/testdata/conformance.yaml` |
| **Node e2e** | `test/e2e_node` | Runs the kubelet plus a runtime on one host | Kubelet features (cgroups, eviction, device plugins, …) |
| **kubeadm e2e**, **DRA e2e**, **Windows node e2e** | `test/e2e_kubeadm`, `test/e2e_dra`, `test/e2e_node_windows` | Specialised | |
| **Scheduler perf** | `test/integration/scheduler_perf` | Benchmarks with YAML workloads | Scheduler performance changes |
| **Fuzz** | `test/fuzz` | Go native fuzzing | Parsers, codecs |
| **kubectl cmd tests** | `test/cmd` | Shell tests against a local apiserver | kubectl behaviour |
| **Soak** | `test/soak` | Long-running tests | |

### 5.1 e2e framework (`test/e2e/framework`)
- `framework.NewDefaultFramework("name")` creates a namespace per test and cleans it up.
- Helpers live in `framework/{pod,node,deployment,service,network,volume,kubectl,…}`.
- Test labels and features: `framework.WithFeatureGate(features.X)`, `WithSerial()`, `WithSlow()`, `WithDisruptive()`.
- Test images (`test/images`, especially **agnhost**, a multipurpose test binary) are referenced via `test/utils/image`.

### 5.2 Typical commands
```bash
# unit test one package, one test
make test WHAT=./pkg/controller/job KUBE_TEST_ARGS='-run TestSyncJob'
go test ./pkg/scheduler/framework/plugins/noderesources/... -run TestFit -v

# integration (needs etcd on PATH)
./hack/install-etcd.sh && export PATH="$(pwd)/third_party/etcd:${PATH}"
make test-integration WHAT=./test/integration/deployment KUBE_TEST_ARGS='-run TestDeploymentRollingUpdate'

# regenerate after API change
./hack/update-codegen.sh && ./hack/update-openapi-spec.sh

# local cluster
./hack/local-up-cluster.sh
export KUBECONFIG=/var/run/kubernetes/admin.kubeconfig
```

## 6. Helper tools in `cmd/`

| Tool | Used by |
|------|---------|
| `cmd/import-boss` | `verify-import-boss.sh`: enforces `.import-restrictions` files |
| `cmd/importverifier` | `verify-imports.sh`: staging must not import `k8s.io/kubernetes` |
| `cmd/preferredimports` | `verify-import-aliases.sh` against `hack/.import-aliases` |
| `cmd/dependencycheck`, `dependencyverifier` | Unwanted dependency checks (`hack/unwanted-dependencies.json`) |
| `cmd/clicheck` | kubectl command conventions |
| `cmd/fieldnamedocscheck` | API doc field names |
| `cmd/gendocs`, `genkubedocs`, `genman`, `genyaml`, `genswaggertypedocs`, `genutils` | Doc generation |
| `cmd/genfeaturegates` | Feature gate list generation |
| `cmd/gotemplate` | Template rendering in scripts |
| `cmd/prune-junit-xml` | CI JUnit post-processing |

## 7. Repository governance files

| File | Purpose |
|------|---------|
| `OWNERS`, `OWNERS_ALIASES` (in every directory) | Prow `/lgtm` (reviewers) and `/approve` (approvers) |
| `.github/PULL_REQUEST_TEMPLATE.md` | PR template (kind, release note, KEP) |
| `CONTRIBUTING.md`, `SUPPORT.md`, `SECURITY_CONTACTS`, `code-of-conduct.md` | Community docs |
| `.generated_files`, `.gitattributes` | Marks generated files (collapsed in GitHub diffs) |
| `AGENTS.md` | Instructions for AI coding agents |
| `CHANGELOG/` | Release notes per minor version |
