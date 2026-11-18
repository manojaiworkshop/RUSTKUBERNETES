# 14 · Auth, Security & Supporting Packages

This document covers the cross-cutting packages that do not belong to a single binary: service account tokens, authorizers, credential providers, Pod Security helpers, probes and feature gates.

## 1. Service accounts & tokens (`pkg/serviceaccount`)

| File | Responsibility |
|------|----------------|
| `jwt.go` | `JWTTokenGenerator` (signs tokens with the SA private key, or through an external signer) and `JWTTokenAuthenticator` (validates the signature, issuer and audience, then calls a `Validator`) |
| `claims.go` | **Bound token** claims: `kubernetes.io: {namespace, serviceaccount{name,uid}, pod{name,uid}, node{name,uid}, secret, warnafter}`, `exp`, `nbf`, `aud`. The validator checks that the bound pod, node or secret still exists with the same UID |
| `legacy.go` | Legacy Secret-based tokens (non-expiring). Usage tracking and invalidation |
| `openidmetadata.go` | Builds the OIDC discovery document and JWKS for the SA issuer |
| `externaljwt/plugin` | gRPC client for an **external JWT signer** (`staging/k8s.io/externaljwt`), so the private key can live in an HSM/KMS |
| `metrics.go` | Token usage metrics (`serviceaccount_*`) |

Token flow: the kubelet asks for a token through `TokenRequest` (`pods/<name>/serviceaccounts/<sa>/token`) for projected volumes (`pkg/kubelet/token`). The token expires (default 1h, auto-refreshed at 80% of its lifetime) and is bound to the pod. When the pod is deleted, the token becomes invalid.

## 2. Authorizers

### 2.1 RBAC (`plugin/pkg/auth/authorizer/rbac`)
- `rbac.go` → `RBACAuthorizer.Authorize(ctx, attrs)`: collects the rules from all `RoleBinding`s and `ClusterRoleBinding`s that match the user or groups (`subject_locator.go`). Returns **Allow** if any rule matches the verb, apiGroup, resource, resourceName or nonResourceURL. Otherwise it returns NoOpinion (RBAC never denies).
- `bootstrappolicy/policy.go`: all default roles: `cluster-admin`, `admin`, `edit`, `view`, `system:node`, `system:kube-scheduler`, `system:kube-controller-manager`, plus per-controller roles (`controller_policy.go`), `system:discovery`, `system:basic-user`, and so on. The apiserver reconciles these on startup.

### 2.2 Node authorizer (`plugin/pkg/auth/authorizer/node`)
- `graph.go`: in-memory graph with vertices for pods, nodes, secrets, configmaps, PVCs, PVs, SAs, ResourceClaims, VolumeAttachments, PodCertificateRequests and so on.
- `graph_populator.go`: informer handlers that keep the graph current.
- `node_authorizer.go`: a request from `system:node:<n>` for `get secret/foo` is allowed only if a path `secret/foo → pod → node/<n>` exists in the graph. Together with the `NodeRestriction` admission plugin, this limits what a compromised node can do.

### 2.3 `pkg/auth/nodeidentifier`
`NodeIdentity(user.Info) (nodeName, isNode)`: recognises `system:node:<name>` in the `system:nodes` group.

## 3. Image pull credentials (`pkg/credentialprovider`)

| File | Responsibility |
|------|----------------|
| `keyring.go` | `DockerKeyring.Lookup(image)`: finds credentials by registry or path match (wildcards such as `*.gcr.io`) |
| `config.go` | Parses `.dockerconfigjson` / `.dockercfg` |
| `secrets/secrets.go` | Builds a keyring from the pod's `imagePullSecrets` |
| `plugin/` | **Kubelet credential provider plugins**: exec binaries configured by `CredentialProviderConfig` (`--image-credential-provider-config`). Responses are cached per image or registry. Supports SA-token-based providers (pass the pod's SA token to the plugin) |
| `provider.go` | Provider registry |

The kubelet also verifies **image pull credentials** for images already cached on the node (`pkg/kubelet/images/pullmanager`), so pods cannot use private images that another pod pulled.

## 4. Pod security helpers

| Package | Responsibility |
|---------|----------------|
| `pkg/securitycontext` | `accessors.go` (pod and container security context accessors), `util.go` (`HasPrivilegedRequest`, `DetermineEffectiveSecurityContext`, `ConvertToRuntimeMaskedPaths/ReadonlyPaths` for `procMount`) |
| `pkg/security/apparmor` | AppArmor profile validation (`validate.go`), field and annotation sync helpers |
| `pkg/kubelet/kuberuntime/security_context*.go`, `kuberuntime_security_profile.go` | Translates securityContext → CRI `LinuxContainerSecurityContext` (capabilities, seccomp, SELinux, runAsUser, user namespaces) |
| `staging/.../pod-security-admission` | Pod Security Standards enforcement (see [05](05-admission-plugins.md) and [12](12-staging-libraries.md#10-pod-security-admission)) |
| `pkg/capabilities` | Process-wide capabilities (allow privileged, host network sources) |
| `pkg/certauthorization` | Shared authorization checks for certificate signers (used by CSR and PodCertificateRequest admission) |

## 5. Probes (`pkg/probe`)

| Package | Implementation |
|---------|----------------|
| `probe.go` | `Result` = `Success` / `Warning` / `Failure` / `Unknown` |
| `exec` | Runs a command in the container through CRI `ExecSync`. Exit code 0 means success |
| `http` | HTTP GET with custom headers. Status 200–399 means success. Follows redirects only on the same host (otherwise a warning). Limited body read |
| `tcp` | TCP connect succeeds |
| `grpc` | `grpc.health.v1.Health/Check` returns SERVING |
| `dialer_*.go` | Probe dialer that sets `SO_LINGER` to avoid TIME_WAIT exhaustion |

## 6. Feature gates (`pkg/features`)

- [`pkg/features/kube_features.go`](../kubernetes/pkg/features/kube_features.go) declares every `featuregate.Feature` constant (with the owner and KEP link in comments) and `defaultVersionedKubernetesFeatureGates`, which holds the per-version history of each gate (`{Version: "1.33", Default: true, PreRelease: featuregate.Beta}`).
- `client_adapter.go` bridges client-go feature gates into the same mechanism.
- At runtime: `utilfeature.DefaultFeatureGate.Enabled(features.X)`. In tests: `featuregatetesting.SetFeatureGateDuringTest(t, gate, features.X, true)`.
- Generated lists for verification: `hack/update-featuregates.sh` / `test/compatibility_lifecycle`.

## 7. Small but important utilities

| Package | Purpose |
|---------|---------|
| `pkg/fieldpath` | `ExtractFieldPathAsString(obj, "metadata.labels['app']")` for the Downward API and env `fieldRef` |
| `pkg/util/taints`, `pkg/util/tolerations` | Parse and merge taints and tolerations (`kubectl taint`, admission) |
| `pkg/util/labels` | Clone and add selectors and labels |
| `pkg/util/hash` | `DeepHashObject` (pod template hashes) |
| `pkg/util/node` | Node address and hostname helpers |
| `pkg/util/oom` | Sets `oom_score_adj` |
| `pkg/util/procfs`, `pkg/util/kernel`, `pkg/util/rlimit` | Linux process, kernel version and limit helpers |
| `pkg/util/flock` | File locks (kubelet `--lock-file`) |
| `pkg/util/goroutinemap` | Run one goroutine per key (used by operations) |
| `pkg/util/removeall` | `RemoveAllOneFilesystem` (safe pod dir cleanup that does not cross mounts) |
| `pkg/util/tail` | Read the last N bytes of logs |
| `pkg/util/interrupt` | Signal-aware handlers |
| `pkg/cluster/ports` | Default ports: `KubeletHealthzPort=10248`, `KubeletPort=10250`, `KubeletReadOnlyPort=10255`, `ProxyStatusPort=10249`, `ProxyHealthzPort=10256`, `KubeControllerManagerPort=10257`, `CloudControllerManagerPort=10258`, `CloudControllerManagerWebhookPort=10260`. (The scheduler's secure port 10259 is defined in the scheduler config packages.) |
