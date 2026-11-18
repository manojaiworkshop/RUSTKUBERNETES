#!/usr/bin/env bash
# Copyright 2026 The rustkubernetes Authors.
# Licensed under the Apache License, Version 2.0.
#
# Runs every check CI runs (like kubernetes/hack/verify-all.sh):
# formatting, lints (warnings are errors), unit + integration tests,
# and the kubectl command-line tests.

set -o errexit -o nounset -o pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "${ROOT}"

echo "+++ cargo fmt --check"
cargo fmt --all -- --check

echo "+++ cargo clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "+++ cargo test"
cargo test --workspace

echo "+++ hack/test-cmd.sh"
hack/test-cmd.sh

echo "+++ all checks passed"
