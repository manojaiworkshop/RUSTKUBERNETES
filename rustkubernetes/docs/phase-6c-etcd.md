# Phase 6c — etcd storage

The API server now stores cluster state in **etcd**, as Kubernetes does. It uses etcd's v3 gRPC API, upstream's key
layout and store semantics, kubeadm's etcd PKI and flags, compaction, health checks and encryption at rest. The
in-memory store stays available for tests and quick experiments (no `--etcd-servers`).

## 1. Pieces

| Piece | Code | Upstream |
|-------|------|----------|
| etcd v3 gRPC client (KV, Watch, Maintenance) | `vendor/go.etcd.io/etcd/client/v3` | `go.etcd.io/etcd/client/v3` (vendored by k8s) |
| etcd3 store | `staging/src/k8s.io/apiserver/src/storage/etcd3.rs` | `k8s.io/apiserver/pkg/storage/etcd3/store.go`, `watcher.go`, `compact.go` |
| Encryption at rest | `staging/src/k8s.io/apiserver/src/storage/value.rs` | `pkg/storage/value`, `pkg/server/options/encryptionconfig` |
| Key layout | `ResourceInfo::storage_prefix` (`registry.rs`) | `StorageFactory.ResourcePrefix` |
| Storage conformance tests | `storage/conformance.rs` (memory and etcd) | `pkg/storage/testing` |
| Flags | `cmd/kube-apiserver/src/app/options.rs` | `--etcd-*`, `--encryption-provider-config` |
| etcd PKI | `cmd/kubeadm` (`init phase certs`) | kubeadm `certs` phase |

The client is generated from etcd's own `.proto` files (copied unchanged, Apache-2.0) with `protox`, so building it
needs no `protoc`. TLS uses rustls with the `ring` provider, like the rest of the project.

## 2. Store semantics

| Operation | etcd request |
|-----------|--------------|
| create | `Txn(If(CreateRevision(key) == 0), Then(Put))`; failure → `AlreadyExists`; resourceVersion = header revision |
| get / list | `Range(key)` / `Range(prefix, prefixEnd)`; object rv = `ModRevision`, list rv = header revision |
| update | `Txn(If(ModRevision(key) == rv), Then(Put), Else(Get))` → `Conflict` / `NotFound`; with no precondition `If(CreateRevision > 0)` |
| delete | `Txn(If(ModRevision == rv), Then(Delete(prevKV)), Else(Get))`; the returned object carries the deletion revision |
| watch | `Watch(prefix, rev+1, WithPrevKV)`; `CreateRevision == ModRevision` → ADDED, other PUT → MODIFIED, DELETE → previous value; compacted → **410 Gone** (informers relist) |
| compaction | every `--etcd-compaction-interval` (5m): `Txn(If(Version(compact_rev_key) == t), Then(Put), Else(Get))` elects one compactor among API servers, which compacts to the revision seen one interval earlier |
| health | `Maintenance.Status`: `[+]etcd ok` in `/healthz` and `/readyz` (a failure → HTTP 500) |

Objects are stored without `resourceVersion` (`PrepareObjectForStorage`). The storage media type is
**`application/json`**. Upstream supports this mode, but its *default* is protobuf; that is the one difference from
upstream (`--storage-media-type` rejects anything else).

## 3. Key layout (as in a kubeadm cluster)

```text
/registry/namespaces/default
/registry/pods/kube-system/coredns-…
/registry/minions/<node>                     nodes
/registry/services/specs/default/kubernetes  services
/registry/services/endpoints/default/kubernetes
/registry/deployments/default/web            no API group in the key
/registry/clusterroles/cluster-admin
/registry/leases/kube-node-lease/<node>
```

`--etcd-prefix` replaces `/registry`, so several clusters can share one etcd. Old in-memory snapshots
(`--data-dir`), which used `/registry/<plural>.<group>/…`, are migrated to this layout when loaded.

## 4. Encryption at rest

`--encryption-provider-config` takes an `EncryptionConfiguration`:

```yaml
apiVersion: apiserver.config.k8s.io/v1
kind: EncryptionConfiguration
resources:
  - resources: [secrets]
    providers:
      - aesgcm:
          keys:
            - name: key1
              secret: <base64 of 32 random bytes>   # head -c 32 /dev/urandom | base64
      - identity: {}
```

- Writes use the first provider. A value is stored as `k8s:enc:aesgcm:v1:key1:` + 12-byte nonce + ciphertext and
  tag, with the **etcd key as additional authenticated data**, so a value copied to another key fails to decrypt.
- Reads pick the provider by prefix. `identity` reads data written before encryption was enabled. To rotate keys, put
  a new key first and keep the old one until every Secret has been rewritten.
- Providers: `aesgcm` (16- or 32-byte keys) and `identity`. `aescbc`, `secretbox` and `kms` are refused with a clear
  message.

## 5. kubeadm PKI and etcd flags

`kubeadm init phase certs all` now also writes:

| File | Subject | Signed by |
|------|---------|-----------|
| `pki/etcd/ca.crt,key` | CN=etcd-ca | self |
| `pki/etcd/server.crt,key`, `peer.crt,key` | CN=<node>; SANs node, localhost, node IPs, 127.0.0.1, ::1; server + client | etcd CA |
| `pki/etcd/healthcheck-client.crt,key` | CN=kube-etcd-healthcheck-client | etcd CA |
| `pki/apiserver-etcd-client.crt,key` | CN=kube-apiserver-etcd-client | etcd CA |

etcd runs in a container (`registry.k8s.io/etcd:3.5.21-0`) with the flags of kubeadm's static pod:
- client and peer TLS with `--client-cert-auth=true` and `--peer-client-cert-auth=true`;
- `--listen-metrics-urls=http://127.0.0.1:2381`, `--snapshot-count=10000`;
- `--experimental-initial-corrupt-check=true`, `--experimental-watch-progress-notify-interval=5s`.

It runs as the invoking user, with its data in a host directory.

## 6. Where it is used

| | Storage |
|-|---------|
| `hack/local-up-cluster.sh` | etcd by default (`STORAGE=memory` for the old mode); mutual TLS when `SECURE=1`; data in `_output/local/etcd`; `ENCRYPT_SECRETS=1` generates an aesgcm config |
| rustkubeadmin clusters | an `etcd` service on the master (before the API server); the etcd image ships in the offline bundle; ports in Advanced → etcd client/peer/metrics |
| `hack/test-e2e-node.sh` | etcd with mutual TLS (`STORAGE=memory` to opt out) |
| `hack/test-cmd.sh` | memory by default; `STORAGE=etcd` for a throwaway etcd |

## 7. Tests

```bash
make test-etcd        # conformance suite + encryption test against a real etcd, then the kubectl suite on etcd
make test             # conformance suite against the memory store, transformer and key-layout tests
```

Verified on this machine:
- The same conformance suite passes on the memory store and on etcd.
- `test-cmd` passes 39/39 on etcd.
- The node e2e suite passes with etcd over mutual TLS.
- etcd refuses clients without a certificate.
- A local cluster keeps Deployments and Secrets across a full restart.
- Encrypted Secrets are unreadable in etcd but readable through the API, and older plaintext Secrets stay readable.
- Console-created clusters run etcd on the master and remove it on delete.

## 8. Not done

- Protobuf storage encoding (JSON is used).
- Multi-member etcd clusters (the code accepts several `--etcd-servers`, but provisioning creates one member) and
  etcd backup/restore from the console.
- KMS and `aescbc`/`secretbox` providers; automatic re-encryption of existing Secrets after a key change
  (`kubectl get secrets -A -o json | kubectl replace -f -` does it).
