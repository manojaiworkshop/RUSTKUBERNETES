# Development guide

## Build, test, run

```bash
cargo build                       # all crates (debug)
cargo test --workspace            # unit + integration tests
hack/test-cmd.sh                  # kubectl compatibility tests (fresh server on a random port)
hack/verify.sh                    # fmt --check, clippy -D warnings, tests, test-cmd
hack/local-up-cluster.sh          # local cluster on :6443; CLEAN=1 wipes state, RELEASE=1 optimizes
```

Logging: `--v 2` (debug) or `RUST_LOG=k8s_apiserver=trace,info`.

## Adding a field to a built-in type

1. Add it to the typed struct in `staging/src/k8s.io/api/src/<group>/v1.rs` (optional fields: `Option<T>`;
   the structs use `#[skip_serializing_none]` + camelCase).
2. Defaults go in `pkg/apis/src/<group>/defaults.rs` (operate on `serde_json::Value`, never re-encode a typed struct
   into storage — the server stores untyped JSON so unknown fields survive).
3. Validation goes in `pkg/apis/src/<group>/validation.rs` and returns `ErrorList` with full field paths.
4. If it is immutable, enforce it in the `validate_update` function of the strategy.

## Adding a new resource

1. Typed struct + `impl_resource!(Kind, "group", "v1", "Kind", "plural", namespaced)` in `k8s-api`.
2. A strategy: either `SimpleStrategy::new(has_status).defaults(..).validate(..).table(..)` or a custom
   `impl Strategy` in `pkg/registry`.
3. Register it in `pkg/controlplane/src/lib.rs::build_registry` with short names, categories and subresources.
4. If clients send it as protobuf, make sure its `generated.proto` is listed in `staging/src/k8s.io/api/build.rs`.
5. Add an integration test in `test/integration/tests/`.

## Adding an admission plugin

Implement `k8s_apiserver::admission::AdmissionPlugin` (`admit` for mutation, `validate` for validation) under
`plugin/src/admission/<name>/` and add it to the chain in `pkg/controlplane::new_server`.

## Writing a controller

Follow any controller in `pkg/controller/src/` (ReplicaSet is the canonical one):

```rust
pub struct Controller { client: Client, store: Store<Foo>, informers: Arc<SharedInformerFactory>, queue: Queue }

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<Foo>();          // shared with other controllers
        let c = Arc::new(Self { client: ctx.client.clone(), store: inf.store(), informers: ctx.informers.clone(), queue: WorkQueue::new() });
        let me = c.clone();
        on_events(inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let me = self.clone();
        run_workers("foo-controller", self.queue.clone(), workers, move |k| { let me = me.clone(); async move { me.sync(&k).await } }).await;
    }

    async fn sync(&self, key: &str) -> SyncResult { /* read store, act through the API, return Ok(None) */ Ok(None) }
}
```

Then add `descriptor!("foo-controller", ["foo"], foo)` to `controller_descriptors()` in `pkg/controller/src/lib.rs`
and an integration test in `test/integration/tests/controller_test.rs` using `start_controllers` (and
`simulate_kubelet` if pods must become ready).

## Testing against the official kubectl

Any kubectl ≥ 1.30 works. Write a kubeconfig with `server: http://127.0.0.1:6443` and an empty user
(see `hack/local-up-cluster.sh`). `kubectl -v=8` shows the exact requests, which is the fastest way to debug
compatibility issues.
