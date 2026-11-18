#!/usr/bin/env bash
# Copyright 2026 The rustkubernetes Authors.
# Licensed under the Apache License, Version 2.0.
#
# Command-line compatibility tests (like kubernetes/test/cmd): starts a fresh
# kube-apiserver on a free port and drives it with the *official* kubectl
# (if installed) and with rkubectl, asserting on the output. The
# kube-controller-manager, kube-scheduler and 2 hollow nodes (kubemark) run too,
# so controller and scheduling behaviour are checked as well.
#
# The API server keeps state in memory; logs and the kubeconfig are written to
# _output/test-cmd/ (overwritten on every run, removed by `make clean`).
#
# Usage: hack/test-cmd.sh

set -o errexit -o nounset -o pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "${ROOT}"

cargo build -q -p kube-apiserver -p kube-controller-manager -p kube-scheduler -p kubemark -p rkubectl

WORK=${ROOT}/_output/test-cmd
mkdir -p "${WORK}"
PORT=${PORT:-$((20000 + RANDOM % 20000))}
"${ROOT}/target/debug/kube-apiserver" --port "${PORT}" --bind-address 127.0.0.1 > "${WORK}/apiserver.log" 2>&1 &
APISERVER_PID=$!
KCM_PID=
SCHED_PID=
KUBEMARK_PID=
trap 'kill ${APISERVER_PID} ${KCM_PID} ${SCHED_PID} ${KUBEMARK_PID} 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:${PORT}/readyz" >/dev/null 2>&1 && break
  sleep 0.1
done

export KUBECONFIG=${WORK}/kubeconfig
cat > "${KUBECONFIG}" <<EOF
apiVersion: v1
kind: Config
clusters: [{name: t, cluster: {server: "http://127.0.0.1:${PORT}"}}]
contexts: [{name: t, context: {cluster: t, user: u}}]
users: [{name: u, user: {}}]
current-context: t
EOF

"${ROOT}/target/debug/kube-controller-manager" --kubeconfig "${KUBECONFIG}" --port $((PORT + 1)) --leader-elect=false > "${WORK}/kcm.log" 2>&1 &
KCM_PID=$!
"${ROOT}/target/debug/kube-scheduler" --kubeconfig "${KUBECONFIG}" --port $((PORT + 2)) --leader-elect=false > "${WORK}/scheduler.log" 2>&1 &
SCHED_PID=$!
"${ROOT}/target/debug/kubemark" --kubeconfig "${KUBECONFIG}" --nodes 2 --job-duration-secs 1 > "${WORK}/kubemark.log" 2>&1 &
KUBEMARK_PID=$!

PASS=0
FAIL=0
# run <description> <expected substring> <command...>
run() {
  local desc=$1 expect=$2
  shift 2
  local out
  out=$("$@" 2>&1 | cat) || true
  if [[ "${out}" == *"${expect}"* ]]; then
    PASS=$((PASS + 1))
    printf '  ok    %s\n' "${desc}"
  else
    FAIL=$((FAIL + 1))
    printf '  FAIL  %s\n        expected: %s\n        got: %s\n' "${desc}" "${expect}" "${out}"
  fi
}

R=${ROOT}/target/debug/rkubectl
echo "+++ rkubectl"
run "version"                 "rustkubernetes"            "${R}" version
run "system namespaces"       "kube-system"               "${R}" get ns
run "apply guestbook"         "deployment.apps/frontend"  "${R}" apply -f examples/guestbook.yaml
run "deployment table"        "frontend"                  "${R}" get deploy -n guestbook
run "service clusterIP"       "10.96."                    "${R}" get svc frontend -n guestbook
run "scale"                   "scaled"                    "${R}" scale deploy/frontend -n guestbook --replicas=5
run "scaled replicas"         "\"replicas\": 5"           "${R}" get deploy frontend -n guestbook -o json
run "unknown type"            "doesn't have a resource type" "${R}" get gizmos
run "delete namespace"        "deleted"                   "${R}" delete ns guestbook
# Namespace deletion is asynchronous (the namespace controller empties it first).
for _ in $(seq 1 60); do
  "${R}" get ns guestbook >/dev/null 2>&1 || break
  sleep 0.5
done

if command -v kubectl >/dev/null 2>&1; then
  K=kubectl
  echo "+++ official kubectl ($(kubectl version --client 2>/dev/null | head -1 | cat))"
  run "server version"        "rustkubernetes"            ${K} version
  run "apply"                 "deployment.apps/frontend created" ${K} apply -f examples/guestbook.yaml
  run "re-apply unchanged"    "unchanged"                 ${K} apply -f examples/guestbook.yaml
  run "typed create (protobuf)" "deployment.apps/api created" ${K} create deploy api -n guestbook --image=busybox
  run "create secret"         "secret/db-pass created"    ${K} create secret generic db-pass -n guestbook --from-literal=pw=s3cret
  run "describe"              "RollingUpdateStrategy"     ${K} describe deploy frontend -n guestbook
  run "label selector"        "frontend"                  ${K} get deploy -n guestbook -l tier=frontend
  run "scale"                 "scaled"                    ${K} scale deploy frontend -n guestbook --replicas=3
  run "patch"                 "patched"                   ${K} patch deploy frontend -n guestbook --type=merge -p '{"spec":{"paused":true}}'
  run "server-side apply"     "serverside-applied"        ${K} apply --server-side --force-conflicts -f examples/guestbook.yaml
  run "server dry-run"        "configmap/dry"             ${K} create cm dry -n guestbook --dry-run=server -o name
  run "validation error"      "is invalid"                ${K} run Bad_Name --image=x -n guestbook
  run "missing namespace"     "namespaces \"nope\" not found" ${K} create cm x -n nope
  run "protected namespace"   "may not be deleted"        ${K} delete ns kube-system
  echo "+++ controllers (kube-controller-manager)"
  run "default serviceaccount" "default"                  bash -c "sleep 2; ${K} get sa -n guestbook"
  run "replicaset created"    "frontend-"                 ${K} get rs -n guestbook
  run "pods created"          "frontend-"                 ${K} get pods -n guestbook
  run "scaling events"        "ScalingReplicaSet"         ${K} get events -n guestbook
  run "rollout history"       "REVISION"                  ${K} rollout history deploy/redis-leader -n guestbook
  run "job pods"              "hello-"                    bash -c "${K} create job hello -n guestbook --image=busybox -- echo hi >/dev/null; sleep 2; ${K} get pods -n guestbook -l job-name=hello"
  run "endpointslice"         "frontend-"                 ${K} get endpointslices -n guestbook
  echo "+++ scheduler + hollow nodes"
  run "hollow nodes ready"    "hollow-node-1   Ready"     ${K} get nodes
  run "rollout completes"     "successfully rolled out"   ${K} rollout status deploy/frontend -n guestbook --timeout=60s
  run "pods running on nodes" "hollow-node-"              ${K} get pods -n guestbook -o wide
  run "endpoints populated"   "10.244."                   ${K} get endpoints frontend -n guestbook
  run "job completes"         "condition met"             ${K} wait --for=condition=complete job/hello -n guestbook --timeout=60s
  run "unschedulable pod"     "Insufficient cpu"          bash -c "${K} run huge -n guestbook --image=x --overrides='{\"spec\":{\"containers\":[{\"name\":\"c\",\"image\":\"x\",\"resources\":{\"requests\":{\"cpu\":\"64\"}}}]}}' >/dev/null; sleep 2; ${K} describe pod huge -n guestbook"
  run "priorityclass"         "system-node-critical"      ${K} get priorityclasses
  run "cascading delete"      "No resources found"        bash -c "${K} delete deploy redis-leader -n guestbook >/dev/null; sleep 3; ${K} get rs,pods -n guestbook -l app=redis"
  run "namespace terminates"  "NotFound"                  bash -c "${K} delete ns guestbook --wait=false >/dev/null; sleep 4; ${K} get ns guestbook"
else
  echo "+++ official kubectl not installed; skipping"
fi

echo
echo "${PASS} passed, ${FAIL} failed"
[[ ${FAIL} -eq 0 ]]
