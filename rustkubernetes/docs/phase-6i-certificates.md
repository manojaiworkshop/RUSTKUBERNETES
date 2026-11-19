# Phase 6i — Certificates: CSR API, TLS bootstrapping, rotation, renewal

Until this phase, every certificate came from `kubeadm init` and was valid for one year with no way to renew it,
so a cluster stopped working after a year. Now:

- **The CSR API**: the cluster signs certificates (`certificates.k8s.io/v1`).
- **Kubelet client certificates**: kubelets renew them on their own (`--rotate-certificates`).
- **Kubelet serving certificates**: kubelets can request them too (`--rotate-server-certificates`).
- **Joining nodes**: new nodes join with a bootstrap token (`kubeadm token`, `kubeadm join`, `--bootstrap-kubeconfig`).
- **Control-plane certificates**: renewed with `kubeadm certs renew` and checked with `kubeadm certs check-expiration`.

```mermaid
sequenceDiagram
  participant K as kubelet
  participant A as kube-apiserver
  participant C as kube-controller-manager
  K->>A: create CSR (signer kube-apiserver-client-kubelet,<br/>O=system:nodes, CN=system:node:NAME)
  Note over A: CSR strategy records the requester;<br/>admission: CertificateSubjectRestriction
  C->>A: SubjectAccessReview: may the requester create<br/>certificatesigningrequests/selfnodeclient (or nodeclient)?
  C->>A: PUT …/approval (Approved, AutoApproved)
  Note over A: CertificateApproval: approver needs<br/>"approve" on signers/<name>
  C->>A: PUT …/status (certificate, signed by --cluster-signing-*)
  Note over A: CertificateSigning: signer needs "sign"
  K->>A: get CSR → certificate
  Note over K: kubelet-client-<ts>.pem, kubelet-client-current.pem → new TLS handshakes use it
```

| Piece | Code | Upstream |
|-------|------|----------|
| `CertificateSigningRequest` type | `staging/src/k8s.io/api/src/certificates.rs` | `k8s.io/api/certificates/v1` |
| CSR parsing, signer names, kubelet CSR rules | `pkg/apis/src/certificates.rs` | `pkg/apis/certificates` |
| Strategies (main, `status`, `approval`), validation, printer | `pkg/registry/src/certificates.rs` | `pkg/registry/certificates/certificates`, `pkg/apis/certificates/validation` |
| `CertificateApproval`, `CertificateSigning`, `CertificateSubjectRestriction` | `plugin/src/admission/certificates` | `plugin/pkg/admission/certificates` |
| Bootstrap-token authenticator | `plugin/src/auth/bootstrap.rs` | `plugin/pkg/auth/authenticator/token/bootstrap` |
| Signing, approving, cleaner controllers; CA | `pkg/controller/src/certificates` | `pkg/controller/certificates/{signer,approver,cleaner,authority}` |
| `bootstrap-signer`, `token-cleaner` controllers | `pkg/controller/src/bootstrap.rs` | `pkg/controller/bootstrap` |
| Kubelet certificate manager, TLS bootstrap | `pkg/kubelet/src/certificate.rs` | `client-go/util/certificate`, `pkg/kubelet/certificate{,/bootstrap}` |
| Swappable TLS certificate (client and server) | `client-go` `util::cert::DynamicCert` | `transport` `GetClientCertificate`, dynamic serving cert |
| `kubeadm token`, `init phase bootstrap-token`, `join`, `certs` | `cmd/kubeadm/src/app/phases/{bootstraptoken,renewal}.rs` | `cmd/kubeadm/app/{cmd/token.go,phases/bootstraptoken,discovery/token,phases/certs/renewal}` |

## 1. API: `certificates.k8s.io/v1` CertificateSigningRequest (`csr`)

- **Create.** `spec.username/uid/groups/extra` are set from the requesting user, whatever the client sent, and the status
  is cleared, so nobody can create a pre-approved request.
- **Validation** (upstream's rules and messages):
  - The PEM request must parse and its signature must verify (`PEM block type must be CERTIFICATE REQUEST`).
  - Usages must be known and appear once.
  - `signerName` must be `domain/path`, not the legacy signer.
  - `expirationSeconds` must be ≥ 600.
  - `Approved`, `Denied` and `Failed` conditions only allow `True`.
- **Subresources.**
  - `approval`: only the conditions change.
  - `status`: the certificate may be set once; `Approved`/`Denied` may not be added, removed or changed.
  - Main update: only metadata changes.
- **`kubectl get csr`**: `NAME AGE SIGNERNAME REQUESTOR REQUESTEDDURATION CONDITION`, for example `Approved,Issued`
  or `60m`.
- **Fixed with this phase.** `AGE` (and every printed duration) now uses Go's `duration.HumanDuration`, as kubectl
  prints it (`5m30s`, `3d4h`), instead of an approximation.
- **Field errors** print values the way Go does: numbers unquoted, byte slices as `[]byte{…}`.

## 2. Admission (all default on, in upstream's order after `StorageObjectInUseProtection`)

| Plugin | Rule | Message |
|--------|------|---------|
| `CertificateApproval` | `approve` on `signers` named the signer (or `<domain>/*`) | `user not permitted to approve requests with signerName "…"` |
| `CertificateSigning` | `sign` for `status` writes that change the certificate or conditions | `user not permitted to sign requests with signerName "…"` |
| `CertificateSubjectRestriction` | no `O=system:masters` for `kubernetes.io/kube-apiserver-client` | `use of kubernetes.io/kube-apiserver-client signer with system:masters group is not allowed` |

The bootstrap policy gains upstream's roles:
- `system:node-bootstrapper`;
- `system:certificates.k8s.io:certificatesigningrequests:{nodeclient,selfnodeclient}`;
- the four `system:certificates.k8s.io:*-approver` roles.

## 3. Controller manager

| Controller | Flags | Behaviour |
|------------|-------|-----------|
| `certificatesigningrequest-signing-controller` (`csrsigning`) | `--cluster-signing-cert-file/--cluster-signing-key-file`, per-signer `--cluster-signing-{kubelet-serving,kubelet-client,kube-apiserver-client,legacy-unknown}-{cert,key}-file`, `--cluster-signing-duration` (8760h) | one loop per signer; checks the signer's rules (kubelet client/serving) and adds `Failed`/`SignerValidationFailure` when broken; otherwise signs |
| `certificatesigningrequest-approving-controller` (`csrapproving`) | — | kubelet client CSRs: SubjectAccessReview for `certificatesigningrequests/selfnodeclient` (CN = requester) or `/nodeclient`; `AutoApproved` with upstream's messages |
| `certificatesigningrequest-cleaner-controller` (`csrcleaner`) | — | hourly; deletes issued/denied/failed after 1h, pending after 24h, expired certificates |
| `bootstrap-signer-controller` (off by default) | `--controllers=*,bootstrapsigner` | `jws-kubeconfig-<id>` in `kube-public/cluster-info` |
| `token-cleaner-controller` (off by default) | `--controllers=*,tokencleaner` | deletes expired bootstrap tokens |

**Signing** follows `PermissiveSigningPolicy`:
- `notBefore` = now − 5 minutes.
- `notAfter` = now + TTL (minus the 5-minute backdate when TTL ≥ 8h), and never later than the CA's own expiry.
- The TTL is `expirationSeconds`, between 10 minutes and `--cluster-signing-duration`.

**Certificate contents** are built like Go's `x509.CreateCertificate`:
- Subject, public key and subjectAltNames are copied byte for byte from the request; other requested extensions are
  dropped.
- `keyUsage` (critical) and `extKeyUsage` come from `spec.usages`.
- `basicConstraints` CA:FALSE (critical); the authority key ID is the CA's.

rcgen can't do this (its subject keeps one value per attribute and it rejects unknown extensions), so the
TBSCertificate is assembled with yasna and signed with ring (ECDSA P-256/P-384, RSA, Ed25519 CA keys). The CA
files are re-read when they change.

The flag uses Go's `time.ParseDuration` (`8760h0m0s`, `3m`), now in `k8s-apimachinery::duration`.

## 4. Kubelet

| Flag | Behaviour |
|------|-----------|
| `--rotate-certificates` | a client certificate manager keeps `<cert-dir>/kubelet-client-<timestamp>.pem` + `kubelet-client-current.pem` (symlink); renewal at 70–84 % of the lifetime (`jitteryDuration(0.7, 0.2)`); the kubeconfig's certificate is the starting point; an expired certificate that cannot be renewed makes the kubelet exit (`The currently active client certificate has expired and the server is responsive, exiting.`) |
| `--bootstrap-kubeconfig` | if `--kubeconfig` is missing or its certificate expired: request a client certificate with the bootstrap credentials (CSR named `node-csr-<digest>` so a restart finds it again; key cached in `kubelet-client.key.tmp`), then write `--kubeconfig` with `default-cluster`/`default-auth`/`default-context` pointing at `kubelet-client-current.pem` |
| `--rotate-server-certificates` | the serving certificate comes from a `kubernetes.io/kubelet-serving` CSR (SANs: hostname, node IP); `--tls-cert-file` is only a fallback; like upstream, these CSRs are **not** auto-approved |
| `--cert-dir` | default `<root-dir>/pki` |

The kubelet's API client and its HTTPS server read their certificate from a `DynamicCert`, so a rotated certificate is
used by the next TLS handshake without a restart. Keys are ECDSA P-256, so the usages are `digital signature` plus
`client auth` or `server auth`, as upstream's kubelet requests for ECDSA keys.

## 5. Bootstrap tokens and `kubeadm`

| Command | |
|---------|--|
| `kube-apiserver --enable-bootstrap-token-auth` | `<id>.<secret>` tokens from `kube-system/bootstrap-token-<id>` Secrets: user `system:bootstrap:<id>`, groups `system:bootstrappers` + `auth-extra-groups` |
| `kubeadm init phase bootstrap-token` | `kubeadm:kubelet-bootstrap`, `kubeadm:get-nodes`, `kubeadm:node-autoapprove-bootstrap`, `kubeadm:node-autoapprove-certificate-rotation`; the `cluster-info` ConfigMap and its anonymous-read Role |
| `kubeadm token create [--ttl 24h0m0s] [--usages] [--groups] [--description] [--print-join-command]`, `list`, `delete`, `generate` | upstream's Secret format and list columns (`TOKEN TTL EXPIRES USAGES DESCRIPTION EXTRA GROUPS`) |
| `kubeadm join <endpoint> --token … --discovery-token-ca-cert-hash sha256:…` | reads `cluster-info` without trusting the server, checks the token's JWS and the CA public-key pin, re-reads it over verified TLS, writes `bootstrap-kubelet.conf` and `pki/ca.crt` |
| `kubeadm certs check-expiration` | upstream's two tables (certificates, CAs) with expiry, residual time, CA and "externally managed" |
| `kubeadm certs renew <name>\|all` | new key, same subject/SANs/usages, now−5m → +1 year; kubeconfigs get their embedded certificate replaced; "Done renewing certificates. You must restart …" |

**Deviation:** `kubeadm join` stops after writing the TLS-bootstrap credentials; the kubelet is started by the node
service (console) or by hand with `--bootstrap-kubeconfig`. Upstream's join also manages the kubelet's systemd unit.

kubeadm now issues certificates like upstream:
- CAs: now → 10 years.
- Leaf certificates: now − 5 minutes → 1 year.

Previously they ran from 1 January of the previous year to the end of the following year.

## 6. Where it is on

- **`hack/local-up-cluster.sh`**:
  - kcm: `--cluster-signing-*` (as upstream's script) and `--controllers=*,bootstrapsigner,tokencleaner`;
  - apiserver: `--enable-bootstrap-token-auth`;
  - kubelet: `--rotate-certificates`;
  - `kubeadm init phase bootstrap-token` runs after start.
- **Console clusters**: the same flags. The master also receives `ca.key` for the signer, and the console runs the
  bootstrap-token phase. Kubelet client certificates now renew themselves.

## 7. Tests

```bash
make test             # CSR validation/strategies, signer (certificate verified with the CA, subject copied byte for byte,
                      # TTL/backdate/CA clamp, key-usage DER), kubelet CSR rules, cleaner deadlines, JWS (checked against
                      # an independent HMAC), bootstrap-token authenticator, kubelet file store, kubeadm renew + check-expiration
make test-e2e-node    # 141 checks; certificates section (33): kubectl create/get/approve/deny, issued certificate authenticates (auth whoami),
                      # system:masters refused, approve without signers/approve refused, SignerValidationFailure,
                      # kubeadm init phase bootstrap-token, token create/list/delete, bootstrap token authenticates,
                      # cluster-info JWS, kubeadm join (and a wrong CA pin refused), a second kubelet TLS-bootstraps and
                      # registers, its serving CSR is approved and served, its client certificate rotates (3m signing
                      # duration) and the node stays Ready, kubeadm certs check-expiration
```

**Not done:**
- `ClusterTrustBundle`, `PodCertificateRequest`;
- the kube-apiserver's own serving certificate rotation (renew with kubeadm and restart);
- automatic approval of kubelet serving CSRs (upstream does not auto-approve them either);
- closing existing connections on rotation (new handshakes use the new certificate, long watches keep the old one
  until they reconnect).
