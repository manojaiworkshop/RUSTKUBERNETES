# Kubernetes Source Code Documentation

> In-depth module and code documentation for the Kubernetes source tree in [`../kubernetes`](../kubernetes).
> Module path: `k8s.io/kubernetes` · Go toolchain: **1.27.4** (`.go-version`) · Release line: **v1.38 development** (latest changelog: `CHANGELOG/CHANGELOG-1.38.md`).

This set of documents explains **what every major module in the repository does, how it works internally, and how the pieces fit together**. It is written for engineers who want to read, debug, or contribute to the Kubernetes codebase.

The repository is very large (~32,000 files). The docs therefore go **module by module**. For each module they cover its responsibility, its main types and functions, its control flow, and where in the source to look next. All source links are relative to this `docs/` folder.

---

## Table of contents

| # | Document | What it covers |
|---|----------|----------------|
| 01 | [Architecture overview](01-architecture-overview.md) | Control plane vs. node, the reconcile model, how a Pod goes from `kubectl apply` to a running container |
| 02 | [Repository layout](02-repository-layout.md) | Every top-level directory, the `staging/` mechanism, Go workspace, import rules |
| 03 | [API types & machinery](03-api-and-type-system.md) | `pkg/apis`, `staging/k8s.io/api`, `apimachinery`, Scheme, conversion, defaulting, validation, code generation |
| 04 | [kube-apiserver](04-kube-apiserver.md) | Startup, server chain (aggregator → kube → apiextensions), handler/filter chain, REST registry, etcd storage, watch cache, authn/authz |
| 05 | [Admission control](05-admission-plugins.md) | Admission framework and **every** in-tree admission plugin, in execution order |
| 06 | [kube-controller-manager](06-kube-controller-manager.md) | Controller framework, leader election, and **every** built-in controller |
| 07 | [kube-scheduler](07-kube-scheduler.md) | Scheduling queue, cache, framework extension points, every in-tree plugin, preemption, gang/pod-group scheduling |
| 08 | [kubelet](08-kubelet.md) | Sync loop, pod workers, PLEG, CRI runtime manager, container manager (cgroups/CPU/memory/devices/DRA), probes, eviction, volumes, status |
| 09 | [kube-proxy](09-kube-proxy.md) | Service/EndpointSlice tracking and the iptables, nftables, IPVS and Windows backends |
| 10 | [kubectl](10-kubectl.md) | Command structure, factory, resource builder, printers, apply/diff, plugins, kuberc |
| 11 | [kubeadm](11-kubeadm.md) | init/join/reset/upgrade workflows and every phase |
| 12 | [Staging libraries](12-staging-libraries.md) | client-go, component-base, apiserver library, apiextensions, aggregator, CRI, CSI, DRA and the other published modules |
| 13 | [Storage & volumes](13-storage-and-volumes.md) | Volume plugin framework, CSI, PV/PVC controllers, attach/detach, kubelet volume manager |
| 14 | [Networking, auth & security packages](14-networking-auth-security.md) | Service accounts & tokens, RBAC/Node authorizers, credential providers, Pod Security, probes, securitycontext |
| 15 | [Build, test & tooling](15-build-test-and-tooling.md) | `Makefile`, `build/`, `hack/`, codegen, verify scripts, unit/integration/e2e/node tests, `cmd/` helper tools |
| 16 | [Developer guide](16-developer-guide.md) | How to add an API field, a controller, a scheduler plugin or an admission plugin, plus debugging tips |
| 17 | [Glossary](17-glossary.md) | Terms used throughout the codebase |

---

## Suggested reading order

1. **New to the code:** 01 → 02 → 03 → 12 (client-go section)
2. **Working on the control plane:** 04 → 05 → 06 → 07
3. **Working on nodes:** 08 → 09 → 13
4. **Working on the CLI or tooling:** 10 → 11 → 15
5. **Before your first PR:** 16

## Conventions used in these docs

- `pkg/...`, `cmd/...` and `staging/...` paths are relative to the repository root `kubernetes/`.
- "**Reconcile**" means comparing the desired state (spec) with the observed state (status) and acting to close the gap.
- Function names are written as `Type.Method` or `package.Func`.
- Mermaid diagrams render on GitHub, GitLab and in VS Code (with a Mermaid extension).
