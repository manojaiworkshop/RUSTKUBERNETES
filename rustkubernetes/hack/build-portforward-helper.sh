#!/usr/bin/env bash
# Copyright 2026 The rustkubernetes Authors.
# Licensed under the Apache License, Version 2.0.
#
# Builds cmd/rustkube-portforward as a fully static binary (glibc, crt-static) and puts it next to the
# other binaries in target/<profile>/, where the kubelet looks for it. It must be static: it runs in a
# container made from the pause image, which has no libc.
#
# Usage: hack/build-portforward-helper.sh [debug|release]

set -o errexit -o nounset -o pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PROFILE=${1:-debug}
FLAGS=()
[[ "${PROFILE}" == "release" ]] && FLAGS=(--release)
TRIPLE=$(rustc -vV | sed -n 's/^host: //p')
RUSTFLAGS="-C target-feature=+crt-static" cargo build -q "${FLAGS[@]}" \
  --manifest-path "${ROOT}/cmd/rustkube-portforward/Cargo.toml" \
  --target "${TRIPLE}" --target-dir "${ROOT}/target/static"
mkdir -p "${ROOT}/target/${PROFILE}"
install -m 0755 "${ROOT}/target/static/${TRIPLE}/${PROFILE}/rustkube-portforward" "${ROOT}/target/${PROFILE}/rustkube-portforward"
