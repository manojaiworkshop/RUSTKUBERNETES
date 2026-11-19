# rustkubernetes developer targets (like kubernetes/build/root/Makefile).

.PHONY: all build release test test-integration test-cmd test-etcd test-e2e-node test-e2e-network admin run-admin verify fmt lint run clean help

all: build

build: ## Build all crates (debug)
	cargo build --workspace
	hack/build-portforward-helper.sh

release: ## Build optimized binaries into target/release
	cargo build --workspace --release
	hack/build-portforward-helper.sh release

test: ## Unit + integration tests
	cargo test --workspace

test-integration: ## Integration tests only
	cargo test -p kube-integration-tests

test-cmd: ## kubectl / rkubectl command-line compatibility tests
	hack/test-cmd.sh

test-etcd: ## Storage conformance + kubectl suite against a real etcd (needs docker)
	hack/test-etcd.sh

test-e2e-node: ## Real kubelet + kube-proxy tests (needs docker and kubectl)
	hack/test-e2e-node.sh

test-e2e-network: ## Two-machine pod network tests: node IPAM, kubenet, flannel (needs docker and kubectl)
	hack/test-e2e-network.sh

verify: ## fmt --check, clippy, tests, test-cmd
	hack/verify.sh

fmt: ## Format all code
	cargo fmt --all

lint: ## Clippy with warnings as errors
	cargo clippy --workspace --all-targets -- -D warnings

admin: ## Build the web console (admin/web → embedded in target/release/rustkubeadmin) and the binaries it installs
	cd admin/web && npm ci --no-audit --no-fund && npm run build
	cargo build --release -p rustkube-admin -p kube-apiserver -p kube-controller-manager -p kube-scheduler -p kubelet -p kube-proxy -p kubeadm -p rkubectl
	hack/build-portforward-helper.sh release

run-admin: ## Run the web console on http://127.0.0.1:8090 (needs RUSTKUBEADMIN_USER / RUSTKUBEADMIN_PASSWORD)
	target/release/rustkubeadmin --listen 127.0.0.1:8090

run: ## Run a local cluster (hack/local-up-cluster.sh)
	hack/local-up-cluster.sh

clean: ## Remove build output and local cluster state
	cargo clean
	rm -rf _output

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-18s %s\n", $$1, $$2}'
