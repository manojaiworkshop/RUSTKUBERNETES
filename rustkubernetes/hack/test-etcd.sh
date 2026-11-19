#!/usr/bin/env bash
# Copyright 2026 The rustkubernetes Authors.
# Licensed under the Apache License, Version 2.0.
#
# etcd storage tests: starts a throwaway etcd (registry.k8s.io/etcd), runs the
# storage conformance suite and encryption-at-rest test against it, then the
# kubectl compatibility suite with the API server on etcd.
#
# Usage: hack/test-etcd.sh
set -o errexit -o nounset -o pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "${ROOT}"
PORT=${PORT:-$((24000 + RANDOM % 4000))}
NAME=rk-etcd-unit-${PORT}
docker run -d --rm --name "${NAME}" --network host registry.k8s.io/etcd:3.5.21-0 etcd --name unit --data-dir /var/lib/etcd \
  --listen-client-urls "http://127.0.0.1:${PORT}" --advertise-client-urls "http://127.0.0.1:${PORT}" \
  --listen-peer-urls "http://127.0.0.1:$((PORT + 1))" --initial-advertise-peer-urls "http://127.0.0.1:$((PORT + 1))" \
  --initial-cluster "unit=http://127.0.0.1:$((PORT + 1))" >/dev/null
trap 'docker rm -f "${NAME}" >/dev/null 2>&1 || true' EXIT
sleep 2
echo "+++ storage conformance against etcd (127.0.0.1:${PORT})"
ETCD_TEST_ENDPOINT="http://127.0.0.1:${PORT}" cargo test -q -p k8s-apiserver storage 2>&1 | grep -E "test result|FAILED|panicked"
echo "+++ kubectl compatibility suite on etcd"
STORAGE=etcd hack/test-cmd.sh
