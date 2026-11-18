#!/usr/bin/env bash
# Copyright 2026 The rustkubernetes Authors.
# Licensed under the Apache License, Version 2.0.
#
# Node end-to-end tests (like kubernetes/test/e2e_node): starts a fresh control
# plane plus a real kubelet backed by Docker and drives it with the official
# kubectl — pods run as real containers, `kubectl logs`/`exec` go through the
# API server to the kubelet. kube-proxy runs in a host-network container with
# NET_ADMIN and its own nftables table (removed at the end) to test Service
# traffic; KUBE_PROXY=0 skips that part.
#
# The cluster is secure, like hack/local-up-cluster.sh: `kubeadm init` creates
# the PKI, the API server serves HTTPS with x509/ServiceAccount authentication and
# RBAC, each component uses its own identity, the kubelet API is authenticated,
# and the CoreDNS add-on provides cluster DNS. Security and DNS are tested too.
#
# Requires: docker (user in the docker group), kubectl, and network access to
# pull busybox:1.36, nginx:1.27-alpine, curlimages/curl:8.11.1, registry.k8s.io/pause:3.10
# and registry.k8s.io/coredns/coredns:v1.12.1 the first time.
#
# Every container the test kubelet creates is labelled with its node name; they
# are deleted through the API at the end, and any leftovers are force-removed.
#
# Usage: hack/test-e2e-node.sh

set -o errexit -o nounset -o pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "${ROOT}"

command -v docker >/dev/null || { echo "docker is required" >&2; exit 1; }
command -v kubectl >/dev/null || { echo "kubectl is required" >&2; exit 1; }
docker version >/dev/null 2>&1 || { echo "cannot talk to Docker (is your user in the docker group?)" >&2; exit 1; }

cargo build -q -p kube-apiserver -p kube-controller-manager -p kube-scheduler -p kubelet -p kube-proxy -p kubeadm
B=${ROOT}/target/debug

WORK=${ROOT}/_output/test-e2e-node
mkdir -p "${WORK}"
# Pick a base port whose whole range (apiserver, KCM, scheduler, kubelet, kube-proxy x2) is free.
port_free() { ! ss -Htan "( sport = :$1 )" 2>/dev/null | grep -q .; }
if [[ -z "${PORT:-}" ]]; then
  for _ in $(seq 1 50); do
    PORT=$((20000 + RANDOM % 12000))
    ok=1
    for off in 0 1 2 3 4 5; do port_free $((PORT + off)) || ok=0; done
    [[ ${ok} == 1 ]] && break
  done
fi
NODE=e2e-node-${PORT}
NS=e2e
PIDS=()
KUBE_PROXY=${KUBE_PROXY:-1}
PROXY_IMAGE=rustkubernetes/kube-proxy-base:24.04
PROXY_CONTAINER=rustkube-proxy-${NODE}
PROXY_TABLE=rustkube-proxy-e2e-${PORT}
cleanup() {
  trap - EXIT
  if [[ "${KUBE_PROXY}" == "1" ]]; then
    docker rm -f "${PROXY_CONTAINER}" >/dev/null 2>&1 || true
    docker run --rm --network host --cap-add NET_ADMIN -v "${B}/kube-proxy:/usr/local/bin/kube-proxy:ro" \
      "${PROXY_IMAGE}" --cleanup --nft-table="${PROXY_TABLE}" >/dev/null 2>&1 || true
  fi
  kubectl delete ns "${NS}" --wait=false >/dev/null 2>&1 || true
  # Give the kubelet a moment to stop and remove the pods' containers.
  for _ in $(seq 1 40); do
    [[ -z "$(docker ps -aq --filter "label=io.rustkubernetes.node=${NODE}")" ]] && break
    sleep 0.5
  done
  kill "${PIDS[@]}" 2>/dev/null || true
  wait 2>/dev/null || true
  local left
  left=$(docker ps -aq --filter "label=io.rustkubernetes.node=${NODE}")
  if [[ -n "${left}" ]]; then
    echo "force-removing containers of ${NODE} still in their termination grace period"
    # shellcheck disable=SC2086
    docker rm -f ${left} >/dev/null
  fi
}
trap cleanup EXIT

HOST=$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Gateway}}')
RUN=${WORK}/run-${PORT}   # fresh PKI per run (removed by `make clean`)
"${B}/kubeadm" init --cert-dir "${RUN}/pki" --kubeconfig-dir "${RUN}" --apiserver-advertise-address "${HOST}" \
  --apiserver-bind-port "${PORT}" --node-name "${NODE}" --node-ip "${HOST}" >/dev/null
export KUBECONFIG=${RUN}/admin.conf
P=${RUN}/pki

"${B}/kube-apiserver" --port "${PORT}" --bind-address "${HOST}" --advertise-address "${HOST}" \
  --tls-cert-file "${P}/apiserver.crt" --tls-private-key-file "${P}/apiserver.key" --client-ca-file "${P}/ca.crt" \
  --authorization-mode Node,RBAC --service-account-key-file "${P}/sa.pub" --service-account-signing-key-file "${P}/sa.key" \
  --kubelet-client-certificate "${P}/apiserver-kubelet-client.crt" --kubelet-client-key "${P}/apiserver-kubelet-client.key" \
  --kubelet-certificate-authority "${P}/ca.crt" > "${WORK}/apiserver.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 100); do
  curl -sfk "https://${HOST}:${PORT}/readyz" >/dev/null 2>&1 && break
  sleep 0.1
done
"${B}/kube-controller-manager" --kubeconfig "${RUN}/controller-manager.conf" --root-ca-file "${P}/ca.crt" \
  --port $((PORT + 1)) --leader-elect=false > "${WORK}/kcm.log" 2>&1 &
PIDS+=($!)
"${B}/kube-scheduler" --kubeconfig "${RUN}/scheduler.conf" --port $((PORT + 2)) --leader-elect=false > "${WORK}/scheduler.log" 2>&1 &
PIDS+=($!)
"${B}/kubelet" --kubeconfig "${RUN}/kubelet.conf" --hostname-override "${NODE}" --node-ip "${HOST}" \
  --port $((PORT + 3)) --root-dir "${WORK}/kubelet" --tls-cert-file "${P}/kubelet.crt" --tls-private-key-file "${P}/kubelet.key" \
  --client-ca-file "${P}/ca.crt" --authorization-mode Webhook --cluster-dns 10.96.0.10 > "${WORK}/kubelet.log" 2>&1 &
PIDS+=($!)

if [[ "${KUBE_PROXY}" == "1" ]]; then
  docker image inspect "${PROXY_IMAGE}" >/dev/null 2>&1 || docker build -q -t "${PROXY_IMAGE}" "${ROOT}/build/kube-proxy" >/dev/null
  docker run -d --name "${PROXY_CONTAINER}" --network host --cap-add NET_ADMIN --cap-add NET_RAW \
    -v "${B}/kube-proxy:/usr/local/bin/kube-proxy:ro" -v "${RUN}/kube-proxy.conf:/etc/kubernetes/kubeconfig:ro" "${PROXY_IMAGE}" \
    --kubeconfig=/etc/kubernetes/kubeconfig --hostname-override="${NODE}" --nft-table="${PROXY_TABLE}" \
    --cluster-cidr="$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Subnet}}')" --hairpin-bridge=docker0 \
    --healthz-bind-address="127.0.0.1:$((PORT + 4))" --metrics-bind-address="127.0.0.1:$((PORT + 5))" >/dev/null
fi

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
K="kubectl -n ${NS}"

echo "+++ node ${NODE} (logs in ${WORK})"
run "node becomes Ready"      "condition met"       kubectl wait --for=condition=Ready "node/${NODE}" --timeout=60s
run "node reports runtime"    "docker://"           kubectl get node "${NODE}" -o jsonpath='{.status.nodeInfo.containerRuntimeVersion}'
run "node lease renewed"      "${NODE}"             kubectl get lease -n kube-node-lease
kubectl create ns "${NS}" >/dev/null

echo "+++ pods"
run "run nginx"               "pod/web created"     ${K} run web --image=nginx:1.27-alpine --labels=app=web
run "pod Ready"               "condition met"       ${K} wait --for=condition=Ready pod/web --timeout=120s
run "pod has bridge IP"       "172."                ${K} get pod web -o jsonpath='{.status.podIP}'
run "real container exists"   "k8s_web_web_${NS}_"  docker ps --filter "label=io.rustkubernetes.node=${NODE}" --format '{{.Names}}'
run "events: Started"         "Started container web" ${K} get events --field-selector involvedObject.name=web

echo "+++ logs / exec (API server -> kubelet)"
run "kubectl logs"            "start worker process" ${K} logs web
run "kubectl exec"            "nginx version"       ${K} exec web -- nginx -v
run "exec multiple args"      "hello e2e"           ${K} exec web -- sh -c 'echo hello $0' e2e
run "exec exit code"          "exit code 7"         ${K} exec web -- sh -c 'exit 7'
run "exec stdin"              "3"                   bash -c "printf 'a\nb\nc\n' | ${K} exec -i web -- wc -l"
run "log needs container (API)" "a container name must be specified" bash -c "${K} run two --image=busybox:1.36 --overrides='{\"spec\":{\"containers\":[{\"name\":\"a\",\"image\":\"busybox:1.36\",\"command\":[\"sleep\",\"600\"]},{\"name\":\"b\",\"image\":\"busybox:1.36\",\"command\":[\"sleep\",\"600\"]}]}}' >/dev/null; sleep 1; kubectl get --raw /api/v1/namespaces/${NS}/pods/two/log"

echo "+++ volumes, env, init containers"
cat <<'EOF' | ${K} apply -f - >/dev/null
apiVersion: v1
kind: ConfigMap
metadata: {name: cfg}
data: {greeting: hello-cm}
---
apiVersion: v1
kind: Secret
metadata: {name: sec}
stringData: {password: s3cr3t}
---
apiVersion: v1
kind: Pod
metadata: {name: vol, labels: {tier: test}}
spec:
  initContainers:
  - {name: init, image: busybox:1.36, command: [sh, -c, "echo from-init > /work/init.txt"], volumeMounts: [{name: work, mountPath: /work}]}
  containers:
  - name: app
    image: busybox:1.36
    command: [sleep, "3600"]
    env:
    - {name: GREETING, valueFrom: {configMapKeyRef: {name: cfg, key: greeting}}}
    - {name: PASSWORD, valueFrom: {secretKeyRef: {name: sec, key: password}}}
    - {name: POD_NAME, valueFrom: {fieldRef: {fieldPath: metadata.name}}}
    - {name: MSG, value: "$(GREETING)-$(POD_NAME)"}
    resources: {limits: {memory: 64Mi, cpu: 250m}}
    volumeMounts:
    - {name: cfg, mountPath: /etc/cfg}
    - {name: work, mountPath: /work}
    - {name: info, mountPath: /etc/info}
  volumes:
  - {name: cfg, configMap: {name: cfg}}
  - {name: work, emptyDir: {}}
  - {name: info, downwardAPI: {items: [{path: labels, fieldRef: {fieldPath: metadata.labels}}]}}
EOF
run "pod with volumes Ready"  "condition met"       ${K} wait --for=condition=Ready pod/vol --timeout=60s
run "env expansion"           "hello-cm-vol"        ${K} exec vol -- sh -c 'echo $MSG'
run "secret env"              "s3cr3t"              ${K} exec vol -- sh -c 'echo $PASSWORD'
run "configMap volume"        "hello-cm"            ${K} exec vol -- cat /etc/cfg/greeting
run "init wrote emptyDir"     "from-init"           ${K} exec vol -- cat /work/init.txt
run "downwardAPI volume"      'tier="test"'         ${K} exec vol -- cat /etc/info/labels
run "memory limit applied"    "67108864"            ${K} exec vol -- cat /sys/fs/cgroup/memory.max

echo "+++ restarts and probes"
${K} run crash --image=busybox:1.36 -- sh -c 'echo boom; exit 3' >/dev/null
run "CrashLoopBackOff"        "CrashLoopBackOff"    bash -c "sleep 8; ${K} get pod crash"
run "logs --previous"         "boom"                ${K} logs crash --previous
cat <<'EOF' | ${K} apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: probes}
spec:
  terminationGracePeriodSeconds: 2
  containers:
  - name: app
    image: busybox:1.36
    command: [sh, -c, "touch /tmp/ok; sleep 4; rm /tmp/ok; sleep 3600"]
    livenessProbe: {exec: {command: [cat, /tmp/ok]}, periodSeconds: 1, failureThreshold: 2}
EOF
run "liveness restarts"       "1"                   bash -c "sleep 14; ${K} get pod probes -o jsonpath='{.status.containerStatuses[0].restartCount}'"
run "Unhealthy event"         "Liveness probe failed" ${K} get events --field-selector involvedObject.name=probes

echo "+++ controllers on a real node"
run "deployment rollout"      "successfully rolled out" bash -c "${K} create deploy dep --image=nginx:1.27-alpine --replicas=2 >/dev/null; ${K} rollout status deploy/dep --timeout=90s"
run "job completes"           "condition met"       bash -c "${K} create job hello --image=busybox:1.36 -- echo hi >/dev/null; ${K} wait --for=condition=complete job/hello --timeout=60s"
run "job logs"                "hi"                  ${K} logs job/hello

if [[ "${KUBE_PROXY}" == "1" ]]; then
  echo "+++ services (kube-proxy, nftables)"
  ${K} create deploy svc --image=nginx:1.27-alpine --replicas=2 >/dev/null
  ${K} rollout status deploy/svc --timeout=90s >/dev/null 2>&1 || true
  for p in $(${K} get pods -l app=svc -o name); do ${K} exec "${p#pod/}" -- sh -c "echo ${p#pod/} > /usr/share/nginx/html/index.html" >/dev/null 2>&1; done
  ${K} expose deploy svc --port=80 --type=NodePort >/dev/null
  sleep 3
  CIP=$(${K} get svc svc -o jsonpath='{.spec.clusterIP}')
  NP=$(${K} get svc svc -o jsonpath='{.spec.ports[0].nodePort}')
  GW=$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Gateway}}')
  run "kube-proxy healthy"       '"nodeEligible":true'  curl -s "127.0.0.1:$((PORT + 4))/healthz"
  run "host -> ClusterIP"        "svc-"                 curl -s -m 3 "http://${CIP}"
  run "pod -> ClusterIP"         "svc-"                 ${K} exec vol -- wget -qO- -T 3 "http://${CIP}"
  run "load balanced (2 pods)"   "2"                    bash -c "for i in \$(seq 20); do curl -s -m 2 http://${CIP}; done | sort -u | wc -l"
  run "NodePort"                 "svc-"                 curl -s -m 3 "http://${GW}:${NP}"
  run "hairpin (pod -> itself)"  "10/10"                bash -c "P=\$(${K} get pods -l app=svc -o jsonpath='{.items[0].metadata.name}'); ${K} exec \$P -- sh -c 'n=0; for i in 1 2 3 4 5 6 7 8 9 10; do wget -qO- -T 2 http://${CIP} >/dev/null && n=\$((n+1)); done; echo \$n/10'"
  run "unknown port rejected"    "rc=7"                 bash -c "curl -s -m 3 http://${CIP}:81; echo rc=\$?"
  run "session affinity"         "1"                    bash -c "${K} patch svc svc -p '{\"spec\":{\"sessionAffinity\":\"ClientIP\"}}' >/dev/null; sleep 2; for i in \$(seq 10); do curl -s -m 2 http://${CIP}; done | sort -u | wc -l"
  run "no endpoints rejected"    "rc=7"                 bash -c "${K} scale deploy svc --replicas=0 >/dev/null; sleep 4; curl -s -m 3 http://${CIP}; echo rc=\$?"
fi

echo "+++ security (TLS, authentication, RBAC)"
run "anonymous gets /readyz"    "ok"                  curl -sk "https://${HOST}:${PORT}/readyz"
run "anonymous denied /api"     "cannot get path"     curl -sk "https://${HOST}:${PORT}/api"
run "admin identity"            "kubeadm:cluster-admins" kubectl auth whoami
"${B}/kubeadm" kubeconfig user --client-name jane --org dev --cert-dir "${P}" --control-plane-endpoint "https://${HOST}:${PORT}" > "${RUN}/jane.conf"
JANE="kubectl --kubeconfig ${RUN}/jane.conf"
run "new user forbidden"        "Forbidden"           ${JANE} get pods -n "${NS}"
kubectl create rolebinding jane-edit --clusterrole=edit --user=jane -n "${NS}" >/dev/null
run "RBAC grant (can-i)"        "yes"                 bash -c "sleep 1; ${JANE} auth can-i create deployments -n ${NS}"
run "RBAC scope (other ns)"     "no"                  ${JANE} auth can-i create deployments -n default
run "edit cannot create roles"  "cannot create resource \"clusterroles\"" ${JANE} create clusterrole sneaky --verb=get --resource=nodes
kubectl create rolebinding jane-admin --clusterrole=admin --user=jane -n "${NS}" >/dev/null
run "admin creates held role"   "created"             bash -c "sleep 1; ${JANE} create role reader --verb=get --resource=pods -n ${NS}"
run "no escalation"             "attempting to grant RBAC permissions not currently held" ${JANE} create role sneaky --verb=get --resource=nodes -n "${NS}"
run "impersonation needs RBAC"  "cannot impersonate"  ${JANE} get pods --as admin -n "${NS}"
run "kubectl create token"      "eyJ"                 kubectl create token default -n "${NS}"
run "kubelet API needs auth"    "401"                 curl -sk -o /dev/null -w "%{http_code}" "https://${HOST}:$((PORT + 3))/pods"
run "root CA published"         "BEGIN CERTIFICATE"   kubectl get cm kube-root-ca.crt -n "${NS}" -o "jsonpath={.data.ca\.crt}"

if [[ "${KUBE_PROXY}" == "1" ]]; then
  echo "+++ cluster DNS (CoreDNS add-on) and in-cluster API access"
  kubectl apply -f cluster/addons/dns/coredns/coredns.yaml >/dev/null
  run "CoreDNS ready"             "successfully rolled out" kubectl rollout status deploy/coredns -n kube-system --timeout=120s
  ${K} scale deploy svc --replicas=1 >/dev/null
  ${K} rollout status deploy/svc --timeout=60s >/dev/null 2>&1 || true
  run "pod resolv.conf"           "search ${NS}.svc.cluster.local" ${K} exec vol -c app -- cat /etc/resolv.conf
  run "Service FQDN resolves"     "Address"             ${K} exec vol -c app -- nslookup "svc.${NS}.svc.cluster.local"
  run "Service short name (HTTP)" "Welcome to nginx"    ${K} exec vol -c app -- wget -qO- -T 3 http://svc
  run "external names forward"    "Name:"               ${K} exec vol -c app -- nslookup -timeout=3 kubernetes.io
  ${K} run curl --image=curlimages/curl:8.11.1 --command -- sleep 3600 >/dev/null
  ${K} wait --for=condition=Ready pod/curl --timeout=120s >/dev/null 2>&1 || true
  SA=/var/run/secrets/kubernetes.io/serviceaccount
  CURL="curl -s --cacert ${SA}/ca.crt -H \"Authorization: Bearer \$(cat ${SA}/token)\" https://kubernetes.default.svc/api/v1/namespaces/${NS}/pods"
  run "pod token denied by RBAC"  "cannot list resource" ${K} exec curl -- sh -c "${CURL}"
  kubectl create rolebinding default-view --clusterrole=view --serviceaccount="${NS}:default" -n "${NS}" >/dev/null
  run "pod token + RBAC (in-cluster API)" "\"kind\":\"PodList\"" bash -c "sleep 1; ${K} exec curl -- sh -c '${CURL}'"
  kubectl delete deploy coredns -n kube-system >/dev/null
fi

echo "+++ deletion"
run "delete pod"              "deleted"             ${K} delete pod web --timeout=30s
run "containers removed"      "gone"                bash -c "sleep 2; docker ps -a --filter name=k8s_web_web_${NS}_ -q | grep -q . && echo still-there || echo gone"

echo
echo "${PASS} passed, ${FAIL} failed"
[[ ${FAIL} -eq 0 ]]
