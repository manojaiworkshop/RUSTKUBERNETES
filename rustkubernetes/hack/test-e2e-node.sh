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
# Storage is etcd (registry.k8s.io/etcd, kubeadm's flags, mutual TLS), like a
# kubeadm cluster; STORAGE=memory uses the in-memory store instead.
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
hack/build-portforward-helper.sh
B=${ROOT}/target/debug

WORK=${ROOT}/_output/test-e2e-node
mkdir -p "${WORK}"
# Pick a base port whose whole range (apiserver, KCM, scheduler, kubelet, kube-proxy x2) is free.
port_free() { ! ss -Htan "( sport = :$1 )" 2>/dev/null | grep -q .; }
if [[ -z "${PORT:-}" ]]; then
  for _ in $(seq 1 50); do
    PORT=$((20000 + RANDOM % 12000))
    ok=1
    for off in 0 1 2 3 4 5 6 7 8 9 10 11; do port_free $((PORT + off)) || ok=0; done
    [[ ${ok} == 1 ]] && break
  done
fi
NODE=e2e-node-${PORT}
NS=e2e
PIDS=()
ETCD_CONTAINER=
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
  if [[ -n "${ETCD_CONTAINER}" ]]; then docker rm -f "${ETCD_CONTAINER}" >/dev/null 2>&1 || true; fi
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

STORAGE_FLAGS=()
if [[ "${STORAGE:-etcd}" == "etcd" ]]; then
  ETCD_PORT=$((PORT + 6))
  ETCD_CONTAINER=rustkube-etcd-e2e-${PORT}
  mkdir -p "${RUN}/etcd" && chmod 700 "${RUN}/etcd"
  docker run -d --rm --name "${ETCD_CONTAINER}" --network host --user "$(id -u):$(id -g)" \
    -v "${RUN}/etcd:/var/lib/etcd" -v "${P}/etcd:/etc/kubernetes/pki/etcd:ro" registry.k8s.io/etcd:3.5.21-0 etcd \
    --name="${NODE}" --data-dir=/var/lib/etcd --advertise-client-urls="https://127.0.0.1:${ETCD_PORT}" \
    --listen-client-urls="https://127.0.0.1:${ETCD_PORT}" --initial-advertise-peer-urls="https://127.0.0.1:$((ETCD_PORT + 1))" \
    --listen-peer-urls="https://127.0.0.1:$((ETCD_PORT + 1))" --initial-cluster="${NODE}=https://127.0.0.1:$((ETCD_PORT + 1))" \
    --listen-metrics-urls="http://127.0.0.1:$((ETCD_PORT + 2))" \
    --cert-file=/etc/kubernetes/pki/etcd/server.crt --key-file=/etc/kubernetes/pki/etcd/server.key --client-cert-auth=true \
    --trusted-ca-file=/etc/kubernetes/pki/etcd/ca.crt --peer-cert-file=/etc/kubernetes/pki/etcd/peer.crt \
    --peer-key-file=/etc/kubernetes/pki/etcd/peer.key --peer-client-cert-auth=true --peer-trusted-ca-file=/etc/kubernetes/pki/etcd/ca.crt \
    --snapshot-count=10000 >/dev/null
  STORAGE_FLAGS=(--etcd-servers "https://127.0.0.1:${ETCD_PORT}" --etcd-cafile "${P}/etcd/ca.crt" \
    --etcd-certfile "${P}/apiserver-etcd-client.crt" --etcd-keyfile "${P}/apiserver-etcd-client.key")
fi

"${B}/kube-apiserver" --port "${PORT}" --bind-address "${HOST}" --advertise-address "${HOST}" "${STORAGE_FLAGS[@]}" \
  --tls-cert-file "${P}/apiserver.crt" --tls-private-key-file "${P}/apiserver.key" --client-ca-file "${P}/ca.crt" \
  --authorization-mode Node,RBAC --enable-admission-plugins NodeRestriction --service-account-key-file "${P}/sa.pub" --service-account-signing-key-file "${P}/sa.key" \
  --kubelet-client-certificate "${P}/apiserver-kubelet-client.crt" --kubelet-client-key "${P}/apiserver-kubelet-client.key" \
  --kubelet-certificate-authority "${P}/ca.crt" \
  --requestheader-client-ca-file "${P}/front-proxy-ca.crt" --requestheader-allowed-names front-proxy-client \
  --requestheader-username-headers X-Remote-User --requestheader-group-headers X-Remote-Group \
  --requestheader-extra-headers-prefix X-Remote-Extra- \
  --proxy-client-cert-file "${P}/front-proxy-client.crt" --proxy-client-key-file "${P}/front-proxy-client.key" \
  --enable-bootstrap-token-auth \
  > "${WORK}/apiserver.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 100); do
  curl -sfk "https://${HOST}:${PORT}/readyz" >/dev/null 2>&1 && break
  sleep 0.1
done
"${B}/kube-controller-manager" --kubeconfig "${RUN}/controller-manager.conf" --root-ca-file "${P}/ca.crt" \
  --enable-hostpath-provisioner --pvclaimbinder-sync-period 2 \
  --cluster-signing-cert-file "${P}/ca.crt" --cluster-signing-key-file "${P}/ca.key" --cluster-signing-duration 3m \
  --controllers '*,bootstrapsigner,tokencleaner' \
  --port $((PORT + 1)) --leader-elect=false > "${WORK}/kcm.log" 2>&1 &
PIDS+=($!)
"${B}/kube-scheduler" --kubeconfig "${RUN}/scheduler.conf" --port $((PORT + 2)) --leader-elect=false > "${WORK}/scheduler.log" 2>&1 &
PIDS+=($!)
"${B}/kubelet" --kubeconfig "${RUN}/kubelet.conf" --hostname-override "${NODE}" --node-ip "${HOST}" \
  --port $((PORT + 3)) --root-dir "${WORK}/kubelet" --tls-cert-file "${P}/kubelet.crt" --tls-private-key-file "${P}/kubelet.key" \
  --client-ca-file "${P}/ca.crt" --authorization-mode Webhook --cluster-dns 10.96.0.10 --rotate-certificates \
  > "${WORK}/kubelet.log" 2>&1 &
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

if [[ -n "${ETCD_CONTAINER}" ]]; then
  echo "+++ etcd storage"
  EC="docker exec ${ETCD_CONTAINER} etcdctl --endpoints https://127.0.0.1:${ETCD_PORT} --cacert /etc/kubernetes/pki/etcd/ca.crt --cert /etc/kubernetes/pki/etcd/healthcheck-client.crt --key /etc/kubernetes/pki/etcd/healthcheck-client.key"
  run "readyz checks etcd"      "[+]etcd ok"          kubectl get --raw "/readyz?verbose"
  run "objects in etcd (layout)" "/registry/minions/${NODE}" bash -c "${EC} get --prefix /registry --keys-only"
  run "deployments key (no group)" "/registry/deployments/${NS}/dep" bash -c "${EC} get --prefix /registry/deployments --keys-only"
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

echo "+++ port-forward, attach, cp (kubectl -> API server -> kubelet)"
cat <<'EOF' | ${K} apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: loopback}
spec:
  containers:
  - name: app
    image: busybox:1.36
    # Listens on the pod's loopback only: reachable by port-forward, not by pod IP.
    command: [sh, -c, "mkdir -p /www && echo only-on-loopback > /www/index.html && httpd -f -p 127.0.0.1:8080 -h /www"]
EOF
${K} wait --for=condition=Ready pod/loopback pod/web --timeout=60s >/dev/null 2>&1 || true
PF1=$((PORT + 9))
PF2=$((PORT + 10))
${K} port-forward pod/web "${PF1}:80" > >(cat > "${WORK}/pf1.log") 2>&1 &
PF_PIDS=($!)
${K} port-forward pod/loopback "${PF2}:8080" > >(cat > "${WORK}/pf2.log") 2>&1 &
PF_PIDS+=($!)
sleep 3
run "port-forward to nginx"          "Welcome to nginx"    curl -s -m 5 "http://127.0.0.1:${PF1}/"
run "port-forward: many requests"    "10"                  bash -c "for i in \$(seq 10); do curl -s -m 5 -o /dev/null -w '%{http_code}\n' http://127.0.0.1:${PF1}/; done | grep -c 200"
run "port-forward: pod loopback"     "only-on-loopback"    curl -s -m 5 "http://127.0.0.1:${PF2}/"
run "port-forward: large response"   "1048576"             bash -c "${K} exec web -- sh -c 'head -c 1048576 /dev/urandom > /usr/share/nginx/html/big'; curl -s -m 10 http://127.0.0.1:${PF1}/big | wc -c"
run "port-forward: kubectl output"   "Forwarding from 127.0.0.1:${PF1} -> 80" cat "${WORK}/pf1.log"
kill "${PF_PIDS[@]}" 2>/dev/null || true
# Clients that upgrade to SPDY directly (no WebSocket tunnel; kubectl before 1.30).
KUBECTL_PORT_FORWARD_WEBSOCKETS=false ${K} port-forward pod/web "${PF1}:80" > >(cat > "${WORK}/pf4.log") 2>&1 &
PF_PIDS=($!)
sleep 3
run "port-forward over plain SPDY"   "Welcome to nginx"    curl -s -m 5 "http://127.0.0.1:${PF1}/"
kill "${PF_PIDS[@]}" 2>/dev/null || true
sleep 1
${K} port-forward pod/web "${PF1}:81" > >(cat > "${WORK}/pf3.log") 2>&1 &
PF_PIDS=($!)
sleep 2
curl -s -m 5 "http://127.0.0.1:${PF1}/" >/dev/null 2>&1 || true
sleep 1
run "port-forward: closed port error" "error forwarding port 81 to pod" cat "${WORK}/pf3.log"
run "port-forward: error detail"     "connect: connection refused" cat "${WORK}/pf3.log"
kill "${PF_PIDS[@]}" 2>/dev/null || true
run "kubectl run -i (attach stdin)"  "attached-ok"         bash -c "echo 'echo attached-ok' | ${K} run att -i --rm --image=busybox:1.36 --restart=Never -- sh 2>&1"
${K} run ticker --image=busybox:1.36 -- sh -c 'i=0; while true; do i=$((i+1)); echo tick-$i; sleep 1; done' >/dev/null
${K} wait --for=condition=Ready pod/ticker --timeout=60s >/dev/null 2>&1 || true
run "kubectl attach (output)"        "tick-"               bash -c "timeout 4 ${K} attach ticker 2>&1"
# Exec and attach over SPDY (clients without WebSocket support).
SPDY="env KUBECTL_REMOTE_COMMAND_WEBSOCKETS=false"
run "exec over SPDY"                 "nginx version"       ${SPDY} ${K} exec web -- nginx -v
run "exec over SPDY: exit code"      "exit code 7"         ${SPDY} ${K} exec web -- sh -c 'exit 7'
run "exec over SPDY: stdin"          "3"                   bash -c "printf 'a\nb\nc\n' | ${SPDY} ${K} exec -i web -- wc -l"
run "exec over SPDY: stderr"         "to-stderr"           ${SPDY} ${K} exec web -- sh -c 'echo to-stderr >&2'
run "attach over SPDY (run -i)"      "spdy-attached"       bash -c "echo 'echo spdy-attached' | ${SPDY} ${K} run att2 -i --rm --image=busybox:1.36 --restart=Never -- sh 2>&1"
echo "copied-into-pod" > "${WORK}/cp-in.txt"
run "kubectl cp into a pod"          "copied-into-pod"     bash -c "${K} cp ${WORK}/cp-in.txt ${NS}/vol:/tmp/cp-in.txt -c app && ${K} exec vol -c app -- cat /tmp/cp-in.txt"
run "kubectl cp out of a pod"        "hello-cm"            bash -c "rm -f ${WORK}/cp-out.txt; ${K} cp ${NS}/vol:/etc/cfg/greeting ${WORK}/cp-out.txt -c app >/dev/null 2>&1; cat ${WORK}/cp-out.txt"

echo "+++ resource metrics (kubelet) and the aggregation layer"
KC="--cacert ${P}/ca.crt --cert ${P}/apiserver-kubelet-client.crt --key ${P}/apiserver-kubelet-client.key"
run "kubelet /metrics/resource: node"  "node_memory_working_set_bytes"  bash -c "curl -s ${KC} https://${HOST}:$((PORT + 3))/metrics/resource"
run "kubelet /metrics/resource: pods"  "pod_cpu_usage_seconds_total{namespace=\"${NS}\",pod=\"vol\"}" bash -c "curl -s ${KC} https://${HOST}:$((PORT + 3))/metrics/resource"
run "kubelet /metrics/resource: HELP"  "# HELP container_cpu_usage_seconds_total [STABLE] Cumulative cpu time consumed by the container in core-seconds" bash -c "curl -s ${KC} https://${HOST}:$((PORT + 3))/metrics/resource"
run "local APIServices"                "v1.apps"                        kubectl get apiservices
run "APIService Local and Available"   "True"                           kubectl get apiservice v1. -o jsonpath='{.status.conditions[?(@.type=="Available")].status}'
run "extension-apiserver-authentication" "X-Remote-User"                kubectl get cm extension-apiserver-authentication -n kube-system -o jsonpath='{.data.requestheader-username-headers}'

echo "+++ persistent volumes (PV controller, VolumeBinding, kubelet, protection)"
VOLDIR=${RUN}/volumes
mkdir -p "${VOLDIR}/static" "${VOLDIR}/local"
cat <<EOF | ${K} apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolume
metadata: {name: static-${PORT}}
spec:
  capacity: {storage: 1Gi}
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  hostPath: {path: ${VOLDIR}/static}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: static}
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: ""
  resources: {requests: {storage: 500Mi}}
EOF
run "static PVC bound"                  "Bound"                     bash -c "for i in \$(seq 20); do p=\$(${K} get pvc static -o jsonpath='{.status.phase}' | cat); [ \"\$p\" = Bound ] && break; sleep 1; done; echo \$p"
run "kubectl get pvc columns"           "static-${PORT}   1Gi        RWO"   ${K} get pvc static
run "kubectl get pv shows the claim"    "${NS}/static"              kubectl get pv "static-${PORT}"
writer() {
  cat <<EOF | ${K} apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: $1}
spec:
  terminationGracePeriodSeconds: 1
  securityContext: {runAsUser: $(id -u)}
  containers:
  - {name: app, image: busybox:1.36, command: [sh, -c, "$3; sleep 3600"], volumeMounts: [{name: data, mountPath: /data}]}
  volumes:
  - {name: data, persistentVolumeClaim: {claimName: $2}}
EOF
}
writer pvc-writer static "echo persisted-data > /data/file"
run "pod with PVC Ready"                "condition met"             ${K} wait --for=condition=Ready pod/pvc-writer --timeout=90s
run "data on the host path"             "persisted-data"            cat "${VOLDIR}/static/file"
run "PVC protection while in use"       "Terminating"               bash -c "${K} delete pvc static --wait=false >/dev/null; sleep 2; ${K} get pvc static"
${K} delete pod pvc-writer --timeout=60s >/dev/null 2>&1 || true
run "PVC removed after its pod"         "NotFound"                  bash -c "sleep 3; ${K} get pvc static 2>&1"
run "Retain: volume Released"           "Released"                  bash -c "sleep 2; kubectl get pv static-${PORT} -o jsonpath='{.status.phase}'"
kubectl apply -f cluster/addons/storage-class/local/default.yaml >/dev/null
run "default StorageClass"              "standard (default)   kubernetes.io/host-path" kubectl get sc
cat <<EOF | ${K} apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: dynamic}
spec:
  accessModes: [ReadWriteOnce]
  resources: {requests: {storage: 100Mi}}
EOF
run "DefaultStorageClass admission"     "standard"                  ${K} get pvc dynamic -o jsonpath='{.spec.storageClassName}'
run "host-path provisioner"             "Bound"                     bash -c "for i in \$(seq 30); do p=\$(${K} get pvc dynamic -o jsonpath='{.status.phase}' | cat); [ \"\$p\" = Bound ] && break; sleep 1; done; echo \$p"
run "ProvisioningSucceeded event"       "Successfully provisioned volume pvc-" ${K} get events --field-selector involvedObject.name=dynamic
DYNPV=$(${K} get pvc dynamic -o jsonpath='{.spec.volumeName}' | cat)
DYNDIR=$(kubectl get pv "${DYNPV}" -o jsonpath='{.spec.hostPath.path}' | cat)
writer dyn-writer dynamic "echo dynamic > /data/f"
run "pod on a provisioned volume"       "condition met"             ${K} wait --for=condition=Ready pod/dyn-writer --timeout=90s
run "provisioned directory written"     "dynamic"                   cat "${DYNDIR}/f"
${K} delete pod dyn-writer --timeout=60s >/dev/null 2>&1 || true
${K} delete pvc dynamic --timeout=60s >/dev/null 2>&1 || true
run "Delete: provisioned volume removed" "NotFound"                 bash -c "for i in \$(seq 30); do kubectl get pv ${DYNPV} >/dev/null 2>&1 || break; sleep 1; done; kubectl get pv ${DYNPV} 2>&1"
run "Delete: directory removed"         "gone"                      bash -c "[ -e ${DYNDIR} ] && echo still-there || echo gone"
cat <<EOF | kubectl apply -f - >/dev/null
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata: {name: local-${PORT}}
provisioner: kubernetes.io/no-provisioner
volumeBindingMode: WaitForFirstConsumer
---
apiVersion: v1
kind: PersistentVolume
metadata: {name: local-${PORT}}
spec:
  capacity: {storage: 1Gi}
  accessModes: [ReadWriteOnce]
  storageClassName: local-${PORT}
  local: {path: ${VOLDIR}/local}
  nodeAffinity:
    required:
      nodeSelectorTerms:
      - matchExpressions: [{key: kubernetes.io/hostname, operator: In, values: [${NODE}]}]
EOF
cat <<EOF | ${K} apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: local}
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: local-${PORT}
  resources: {requests: {storage: 1Gi}}
EOF
run "WaitForFirstConsumer: Pending"     "waiting for first consumer to be created before binding" bash -c "sleep 3; ${K} describe pvc local"
writer local-writer local "echo local-volume > /data/f"
run "local PV bound at scheduling"      "condition met"             ${K} wait --for=condition=Ready pod/local-writer --timeout=90s
run "local volume bound"                "local-${PORT}"             ${K} get pvc local -o jsonpath='{.spec.volumeName}'
run "local volume written"              "local-volume"              cat "${VOLDIR}/local/f"
${K} delete pod local-writer --timeout=60s >/dev/null 2>&1 || true
cat <<EOF | ${K} apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: affinity-miss}
spec:
  nodeSelector: {kubernetes.io/hostname: ${NODE}}
  containers: [{name: app, image: busybox:1.36, command: [sleep, "3600"], volumeMounts: [{name: d, mountPath: /d}]}]
  volumes: [{name: d, persistentVolumeClaim: {claimName: missing}}]
EOF
run "missing PVC blocks scheduling"     'nodes are available: persistentvolumeclaim "missing" not found' bash -c "sleep 3; ${K} get events --field-selector involvedObject.name=affinity-miss"
${K} delete pod affinity-miss --wait=false >/dev/null 2>&1 || true
kubectl delete pv "static-${PORT}" "local-${PORT}" --wait=false >/dev/null 2>&1 || true
kubectl delete sc "local-${PORT}" standard >/dev/null 2>&1 || true

echo "+++ node authorization (Node authorizer, NodeRestriction)"
KL="kubectl --kubeconfig ${RUN}/kubelet.conf"
${K} create secret generic unrelated --from-literal=x=y >/dev/null
kubectl create -f - >/dev/null <<EOF
apiVersion: v1
kind: Node
metadata: {name: other-${NODE}}
EOF
run "binding system:node has no subjects" "0"          bash -c "kubectl get clusterrolebinding system:node -o jsonpath='{.subjects}' | wc -c"
run "node reads its own Node"       "node/${NODE}"      ${KL} get node "${NODE}" -o name
run "node cannot list all nodes"    "cannot read all nodes, only its own Node object" ${KL} get nodes
run "node cannot read another Node" "cannot read 'other-${NODE}', only its own Node object" ${KL} get node "other-${NODE}"
run "secret used by its pod"        "czNjcjN0"          ${KL} get secret sec -n "${NS}" -o jsonpath='{.data.password}'
run "configmap used by its pod"     "hello-cm"          ${KL} get configmap cfg -n "${NS}" -o jsonpath='{.data.greeting}'
run "unrelated secret denied"       "no relationship found between node '${NODE}' and this object" ${KL} get secret unrelated -n "${NS}"
run "listing secrets denied"        "No Object name found" ${KL} get secrets -n "${NS}"
run "pods: list needs nodeName"     "can only list/watch pods with spec.nodeName field selector" ${KL} get pods -A
run "pods: own pods listed"         "vol"               ${KL} get pods -A --field-selector "spec.nodeName=${NODE}"
run "own lease"                     "lease.coordination.k8s.io/${NODE}" ${KL} get lease "${NODE}" -n kube-node-lease -o name
run "leases elsewhere denied"       "can only access leases in the \"kube-node-lease\" system namespace" ${KL} get lease x -n kube-system
run "NodeRestriction: other node"   "is not allowed to modify node \"other-${NODE}\"" bash -c "kubectl get node other-${NODE} -o json | cat > ${RUN}/other-node.json; ${KL} replace --raw /api/v1/nodes/other-${NODE} -f ${RUN}/other-node.json"
run "NodeRestriction: role label"   "is not allowed to modify labels: node-role.kubernetes.io/worker" ${KL} label node "${NODE}" node-role.kubernetes.io/worker=
run "NodeRestriction: taints"       "is not allowed to modify taints" ${KL} taint node "${NODE}" e2e=x:NoSchedule
run "NodeRestriction: kubelet label" "labeled"          ${KL} label node "${NODE}" node.kubernetes.io/pool=e2e
run "NodeRestriction: mirror pods only" "can only create mirror pods" ${KL} run rogue -n "${NS}" --image=busybox:1.36
run "admin may still label nodes"   "labeled"           kubectl label node "${NODE}" node-role.kubernetes.io/worker=
kubectl delete node "other-${NODE}" >/dev/null 2>&1 || true

echo "+++ certificates (CSR API, signers, approval, TLS bootstrap, rotation)"
CD=${RUN}/csr
mkdir -p "${CD}"
# csr <name> <subject> <signer> <usages-json> [expirationSeconds]
csr() {
  openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "${CD}/$1.key" -subj "$2" -out "${CD}/$1.csr" 2>/dev/null
  kubectl create -f - <<EOF
apiVersion: certificates.k8s.io/v1
kind: CertificateSigningRequest
metadata: {name: $1}
spec:
  request: $(base64 -w0 < "${CD}/$1.csr")
  signerName: $3
  usages: $4
${5:+  expirationSeconds: $5}
EOF
}
export -f csr
export CD
run "create CSR"                    "certificatesigningrequest.certificates.k8s.io/jane-${PORT} created" csr "jane-${PORT}" "/O=dev/CN=jane" kubernetes.io/kube-apiserver-client '["client auth"]' 3600
run "kubectl get csr columns"       "60m" bash -c "kubectl get csr jane-${PORT} | cat"
run "requestor is the creator"      "kubernetes-admin" kubectl get csr "jane-${PORT}" -o jsonpath='{.spec.username}'
run "pending until approved"        "Pending" bash -c "sleep 1; kubectl get csr jane-${PORT} --no-headers | cat"
run "kubectl certificate approve"   "certificatesigningrequest.certificates.k8s.io/jane-${PORT} approved" kubectl certificate approve "jane-${PORT}"
run "signer issues the certificate" "Approved,Issued" bash -c "for i in \$(seq 1 30); do kubectl get csr jane-${PORT} --no-headers | grep -q Issued && break; sleep 0.5; done; kubectl get csr jane-${PORT} --no-headers | cat"
kubectl get csr "jane-${PORT}" -o jsonpath='{.status.certificate}' | base64 -d > "${CD}/jane.crt"
run "issued cert authenticates"     "jane" kubectl --kubeconfig /dev/null --server "https://${HOST}:${PORT}" --certificate-authority "${P}/ca.crt" --client-certificate "${CD}/jane.crt" --client-key "${CD}/jane-${PORT}.key" auth whoami
run "issued cert subject"           "O = dev, CN = jane" bash -c "openssl x509 -in ${CD}/jane.crt -noout -subject"
run "approval condition"            "KubectlApprove" kubectl get csr "jane-${PORT}" -o jsonpath='{.status.conditions[0].reason}'
run "system:masters refused"        "use of kubernetes.io/kube-apiserver-client signer with system:masters group is not allowed" csr "evil-${PORT}" "/O=system:masters/CN=evil" kubernetes.io/kube-apiserver-client '["client auth"]'
csr "deny-${PORT}" /CN=deny kubernetes.io/kube-apiserver-client '["client auth"]' >/dev/null
run "kubectl certificate deny"      "denied" kubectl certificate deny "deny-${PORT}"
run "denied CSR not issued"         "Denied" bash -c "sleep 1; kubectl get csr deny-${PORT} --no-headers | cat"
csr "bad-${PORT}" /O=system:nodes/CN=system:node:x kubernetes.io/kubelet-serving '["client auth"]' >/dev/null
kubectl certificate approve "bad-${PORT}" >/dev/null
run "signer validation failure"     "SignerValidationFailure" bash -c "sleep 2; kubectl get csr bad-${PORT} -o jsonpath='{.status.conditions[*].reason}'"
kubectl create clusterrole "csr-approver-${PORT}" --verb=get,list,update --resource=certificatesigningrequests,certificatesigningrequests/approval >/dev/null
kubectl create clusterrolebinding "csr-approver-${PORT}" --clusterrole="csr-approver-${PORT}" --user=bob >/dev/null
csr "bob-${PORT}" /CN=x kubernetes.io/kube-apiserver-client '["client auth"]' >/dev/null
run "approve needs signers/approve" "user not permitted to approve requests with signerName \"kubernetes.io/kube-apiserver-client\"" kubectl --as bob certificate approve "bob-${PORT}"
kubectl delete clusterrole,clusterrolebinding "csr-approver-${PORT}" >/dev/null 2>&1 || true

# TLS bootstrapping of a second kubelet with a bootstrap token.
NODE2=${NODE}-b
J2=${RUN}/node2
run "kubeadm init phase bootstrap-token" "Configured RBAC rules to allow certificate rotation for all node client certificates" "${B}/kubeadm" init phase bootstrap-token --kubeconfig "${KUBECONFIG}"
JOIN=$("${B}/kubeadm" token create --kubeconfig "${KUBECONFIG}" --print-join-command --description "e2e join")
TOKEN=$(echo "${JOIN}" | sed -n 's/.*--token \([^ ]*\).*/\1/p')
run "kubeadm token create"          "kubeadm join ${HOST}:${PORT} --token " echo "${JOIN}"
run "kubeadm token list"            "e2e join" "${B}/kubeadm" token list --kubeconfig "${KUBECONFIG}"
run "bootstrap token authenticates" "system:bootstrap:${TOKEN%%.*}" kubectl --kubeconfig /dev/null --server "https://${HOST}:${PORT}" --certificate-authority "${P}/ca.crt" --token "${TOKEN}" auth whoami
run "cluster-info signed (JWS)"     "jws-kubeconfig-${TOKEN%%.*}" bash -c "for i in \$(seq 1 20); do kubectl get cm cluster-info -n kube-public -o yaml | grep -q jws-kubeconfig && break; sleep 0.5; done; kubectl get cm cluster-info -n kube-public -o yaml | cat"
run "kubeadm join (discovery)"      "Cluster info signature and contents are valid" bash -c "${B}/${JOIN} --kubeconfig-dir ${J2}"
run "join refuses a wrong CA pin"   "none of the public keys" "${B}/kubeadm" join "${HOST}:${PORT}" --token "${TOKEN}" --discovery-token-ca-cert-hash sha256:0000000000000000000000000000000000000000000000000000000000000000 --kubeconfig-dir "${J2}-bad"
"${B}/kubelet" --bootstrap-kubeconfig "${J2}/bootstrap-kubelet.conf" --kubeconfig "${J2}/kubelet.conf" --hostname-override "${NODE2}" \
  --node-ip "${HOST}" --port $((PORT + 9)) --root-dir "${WORK}/kubelet2" --cert-dir "${J2}/pki" --rotate-certificates --rotate-server-certificates \
  --client-ca-file "${P}/ca.crt" --authorization-mode Webhook --register-with-taints e2e=bootstrap:NoSchedule > "${WORK}/kubelet2.log" 2>&1 &
KUBELET2=$!
PIDS+=("${KUBELET2}")
run "TLS bootstrap: node registers" "condition met" bash -c "for i in \$(seq 1 90); do kubectl get node ${NODE2} >/dev/null 2>&1 && break; sleep 1; done; kubectl wait --for=condition=Ready node/${NODE2} --timeout=90s"
run "bootstrap CSR auto-approved"   "AutoApproved" bash -c "kubectl get csr -o jsonpath='{range .items[*]}{.spec.username} {.status.conditions[0].reason}{\"\\n\"}{end}' | grep system:bootstrap:${TOKEN%%.*}"
run "kubeconfig written by kubelet" "kubelet-client-current.pem" cat "${J2}/kubelet.conf"
run "kubelet client cert identity"  "O = system:nodes, CN = system:node:${NODE2}" bash -c "openssl x509 -in ${J2}/pki/kubelet-client-current.pem -noout -subject"
run "serving CSR pending"           "kubernetes.io/kubelet-serving" bash -c "for i in \$(seq 1 20); do kubectl get csr --no-headers | grep kubelet-serving | grep -q system:node:${NODE2} && break; sleep 0.5; done; kubectl get csr --no-headers | grep system:node:${NODE2} | cat"
SERVING=$(kubectl get csr -o jsonpath='{range .items[*]}{.metadata.name} {.spec.signerName} {.spec.username}{"\n"}{end}' | awk -v u="system:node:${NODE2}" '$2=="kubernetes.io/kubelet-serving" && $3==u {print $1; exit}')
run "approve serving CSR"           "approved" kubectl certificate approve "${SERVING}"
run "kubelet serves the new cert"   "401" bash -c "for i in \$(seq 1 30); do c=\$(curl -s --cacert ${P}/ca.crt -o /dev/null -w '%{http_code}' https://${HOST}:$((PORT + 9))/pods); [[ \$c == 401 ]] && break; sleep 0.5; done; echo \$c"
run "client certificate rotated"    "rotated" bash -c "for i in \$(seq 1 150); do [[ \$(ls ${J2}/pki/kubelet-client-2*.pem 2>/dev/null | wc -l) -ge 2 ]] && echo rotated && break; sleep 1; done"
run "node still Ready after rotation" "True" bash -c "sleep 12; kubectl get node ${NODE2} -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'"
run "self renewal auto-approved"    "Auto approving self kubelet client certificate after SubjectAccessReview." bash -c "kubectl get csr -o jsonpath='{range .items[*]}{.spec.username} {.status.conditions[0].message}{\"\\n\"}{end}' | grep system:node:${NODE2}"
run "kubeadm token delete"          "deleted" "${B}/kubeadm" token delete --kubeconfig "${KUBECONFIG}" "${TOKEN%%.*}"
run "kubeadm certs check-expiration" "apiserver-kubelet-client" "${B}/kubeadm" certs check-expiration --cert-dir "${P}" --kubeconfig-dir "${RUN}"
kill "${KUBELET2}" 2>/dev/null || true
kubectl delete node "${NODE2}" >/dev/null 2>&1 || true

echo "+++ deletion"
run "delete pod"              "deleted"             ${K} delete pod web --timeout=30s
run "containers removed"      "gone"                bash -c "sleep 2; docker ps -a --filter name=k8s_web_web_${NS}_ -q | grep -q . && echo still-there || echo gone"

echo
echo "${PASS} passed, ${FAIL} failed"
[[ ${FAIL} -eq 0 ]]
