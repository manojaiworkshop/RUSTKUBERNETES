#!/usr/bin/env bash
# Copyright 2026 The rustkubernetes Authors.
# Licensed under the Apache License, Version 2.0.
#
# Builds and runs a local single-machine cluster (like kubernetes/hack/local-up-cluster.sh).
#
# Runs: kube-apiserver, kube-controller-manager, kube-scheduler, a real kubelet
# (runs pods as Docker containers; KUBELET=0 to disable), kube-proxy (Service
# traffic via nftables; KUBE_PROXY=0 to disable), the CoreDNS add-on (DNS=0 to
# disable) and HOLLOW_NODES hollow nodes (kubemark; fake kubelets) — default 0.
#
# Security (SECURE=1, the default): `kubeadm init` creates a CA, certificates and
# kubeconfigs under ${OUT}/pki and ${OUT}/*.conf; the API server serves HTTPS with
# x509 + ServiceAccount token authentication and RBAC; every component uses its
# own identity; pods get projected ServiceAccount tokens; the kubelet API requires
# TLS client certificates or tokens and authorizes via SubjectAccessReview.
# The API server listens on the Docker bridge gateway (e.g. 172.17.0.1) so pods
# reach it through the `kubernetes` Service. SECURE=0 runs the old insecure HTTP
# development mode on 127.0.0.1 (no auth, no DNS add-on).
#
# Storage (STORAGE=etcd, the default): etcd runs in a container
# (registry.k8s.io/etcd, kubeadm's flags; data in ${OUT}/etcd) — with TLS client
# certificates in SECURE mode — and the API server stores everything there under
# /registry, like kubeadm clusters.
#
# Pod network (NET_PLUGIN): by default pods use Docker's default bridge (docker0), like the dockershim
# without a network plugin. NET_PLUGIN=kubenet does what a multi-node cluster does: kube-controller-manager
# allocates each node a pod CIDR out of POD_CIDR (--allocate-node-cidrs), the kubelet puts pods on the bridge
# cbr0 on that CIDR (--network-plugin=kubenet) and the flannel add-on masquerades traffic leaving the cluster
# (and would route between nodes). flannel changes the host's iptables (FLANNEL-* chains), like on any node.
#
# kube-proxy needs CAP_NET_ADMIN on the host, so — like a kube-proxy DaemonSet —
# it runs in a container (image build/kube-proxy, built on first use) with
# --network host --cap-add NET_ADMIN. Its rules live in the nftables table
# "rustkube-proxy" only, and are removed when the cluster stops.
#
# Usage:
#   hack/local-up-cluster.sh                 # build (debug) + run on :6443, state in _output/local/data
#   API_PORT=16443 hack/local-up-cluster.sh
#   RELEASE=1 hack/local-up-cluster.sh       # optimized build
#   CLEAN=1 hack/local-up-cluster.sh         # wipe stored state (and certificates) first
#   SECURE=0 KUBELET=0 HOLLOW_NODES=3 hack/local-up-cluster.sh   # phase-3 style, no Docker, no auth
#   KUBELET_PORT=10250 NODE_NAME=mynode DOCKER_HOST=unix:///var/run/docker.sock
#   KUBE_PROXY=0 hack/local-up-cluster.sh               # no Service proxying (no host firewall changes)
#   PROXY_TABLE=rk-proxy-2 ...                           # second cluster on the same host
#   STORAGE=memory hack/local-up-cluster.sh              # in-memory store (+ snapshot) instead of etcd
#   ETCD_PORT=2379 ...                                   # etcd client port (peer: +1, metrics: +2)
#   ENCRYPT_SECRETS=1 hack/local-up-cluster.sh           # Secrets encrypted at rest (aesgcm, key in ${OUT})
#   ENABLE_HOSTPATH_PROVISIONER=false ...                # no host-path provisioner / default StorageClass
#   METRICS=0 hack/local-up-cluster.sh                  # no metrics-server add-on (default: with DNS)
#   NET_PLUGIN=kubenet hack/local-up-cluster.sh          # node pod CIDRs + cbr0 + flannel (POD_CIDR=10.244.0.0/16)
#
# Then, in another terminal:
#   export KUBECONFIG=$(pwd)/_output/local/kubeconfig   # cluster-admin (admin.conf)
#   kubectl get nodes
#   kubectl auth whoami

set -o errexit -o nounset -o pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "${ROOT}"

SECURE=${SECURE:-1}
KUBELET=${KUBELET:-1}
if [[ "${SECURE}" == "1" ]]; then
  # Pods must reach the API server: listen on the Docker bridge gateway.
  DEFAULT_HOST=$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null || echo 127.0.0.1)
else
  DEFAULT_HOST=127.0.0.1
fi
API_HOST=${API_HOST:-${DEFAULT_HOST}}
API_PORT=${API_PORT:-6443}
OUT=${OUT:-${ROOT}/_output/local}
DATA_DIR=${DATA_DIR:-${OUT}/data}
LOG_DIR=${LOG_DIR:-${OUT}/logs}
PKI=${OUT}/pki
SERVICE_CIDR=${SERVICE_CIDR:-10.96.0.0/12}
NET_PLUGIN=${NET_PLUGIN:-}
POD_CIDR=${POD_CIDR:-10.244.0.0/16}
KUBENET_BRIDGE=${KUBENET_BRIDGE:-cbr0}
FLANNEL_BACKEND=${FLANNEL_BACKEND:-host-gw}
NODE_PORT_RANGE=${NODE_PORT_RANGE:-30000-32767}
CLUSTER_DNS=${CLUSTER_DNS:-10.96.0.10}
CLUSTER_DOMAIN=${CLUSTER_DOMAIN:-cluster.local}
NODE_NAME=${NODE_NAME:-$(hostname | tr '[:upper:]' '[:lower:]')}
VERBOSITY=${VERBOSITY:-1}

PROFILE=debug
CARGO_FLAGS=()
if [[ "${RELEASE:-0}" == "1" ]]; then
  PROFILE=release
  CARGO_FLAGS+=(--release)
fi
BIN=${ROOT}/target/${PROFILE}

if [[ "${CLEAN:-0}" == "1" ]]; then
  echo "+++ Removing previous state in ${DATA_DIR} and ${PKI}"
  rm -rf "${DATA_DIR}" "${PKI}" "${OUT}"/*.conf "${OUT}/etcd"
fi
mkdir -p "${DATA_DIR}" "${LOG_DIR}"

echo "+++ Building (${PROFILE})"
cargo build "${CARGO_FLAGS[@]}" -p kube-apiserver -p kube-controller-manager -p kube-scheduler -p kubemark -p kubelet -p kube-proxy -p kubeadm -p rkubectl
hack/build-portforward-helper.sh "${PROFILE}"

KUBECONFIG_FILE=${OUT}/kubeconfig
if [[ "${SECURE}" == "1" ]]; then
  echo "+++ Generating certificates and kubeconfigs (kubeadm init) for https://${API_HOST}:${API_PORT}"
  "${BIN}/kubeadm" init --cert-dir "${PKI}" --kubeconfig-dir "${OUT}" \
    --apiserver-advertise-address "${API_HOST}" --apiserver-bind-port "${API_PORT}" \
    --node-name "${NODE_NAME}" --node-ip "${API_HOST}" --service-dns-domain "${CLUSTER_DOMAIN}" \
    --apiserver-cert-extra-sans "127.0.0.1,${EXTRA_SANS:-}" | sed 's/^/    /'
  cp "${OUT}/admin.conf" "${KUBECONFIG_FILE}"
  ADMIN_CONF=${OUT}/admin.conf
  KCM_CONF=${OUT}/controller-manager.conf
  SCHED_CONF=${OUT}/scheduler.conf
  KUBELET_CONF=${OUT}/kubelet.conf
  PROXY_CONF=${OUT}/kube-proxy.conf
  SCHEME=https
else
  cat > "${KUBECONFIG_FILE}" <<EOF
apiVersion: v1
kind: Config
clusters:
- name: local-up-cluster
  cluster:
    server: http://${API_HOST}:${API_PORT}
contexts:
- name: local-up-cluster
  context:
    cluster: local-up-cluster
    user: admin
current-context: local-up-cluster
users:
- name: admin
  user: {}
EOF
  ADMIN_CONF=${KUBECONFIG_FILE}
  KCM_CONF=${KUBECONFIG_FILE}
  SCHED_CONF=${KUBECONFIG_FILE}
  KUBELET_CONF=${KUBECONFIG_FILE}
  PROXY_CONF=${KUBECONFIG_FILE}
  SCHEME=http
fi

PIDS=()
ETCD_CONTAINER=
PROXY_CONTAINER=
PROXY_IMAGE=${PROXY_IMAGE:-rustkubernetes/kube-proxy-base:24.04}
PROXY_TABLE=${PROXY_TABLE:-rustkube-proxy}   # use a different table per cluster on one host
cleanup() {
  trap - EXIT INT TERM
  echo
  echo "+++ Stopping cluster"
  if [[ -n "${ETCD_CONTAINER}" ]]; then
    docker rm -f "${ETCD_CONTAINER}" >/dev/null 2>&1 || true   # data stays in ${OUT}/etcd
  fi
  if [[ -n "${PROXY_CONTAINER}" ]]; then
    docker rm -f "${PROXY_CONTAINER}" >/dev/null 2>&1 || true
    # Remove the Service rules from the host firewall.
    docker run --rm --network host --cap-add NET_ADMIN -v "${BIN}/kube-proxy:/usr/local/bin/kube-proxy:ro" \
      "${PROXY_IMAGE}" --cleanup --nft-table="${PROXY_TABLE}" >/dev/null 2>&1 || true
  fi
  for pid in "${PIDS[@]}"; do
    kill "${pid}" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  if [[ "${NET_PLUGIN}" == "kubenet" ]]; then
    # The bridge is recreated from the node's pod CIDR on the next start.
    docker network rm "${KUBENET_BRIDGE}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

SECURE_FLAGS=()
if [[ "${SECURE}" == "1" ]]; then
  SECURE_FLAGS=(
    --tls-cert-file="${PKI}/apiserver.crt" --tls-private-key-file="${PKI}/apiserver.key"
    --client-ca-file="${PKI}/ca.crt"
    --authorization-mode=Node,RBAC --enable-admission-plugins=NodeRestriction
    --service-account-key-file="${PKI}/sa.pub" --service-account-signing-key-file="${PKI}/sa.key"
    --service-account-issuer="https://kubernetes.default.svc.${CLUSTER_DOMAIN}"
    --kubelet-client-certificate="${PKI}/apiserver-kubelet-client.crt" --kubelet-client-key="${PKI}/apiserver-kubelet-client.key"
    --kubelet-certificate-authority="${PKI}/ca.crt"
    # Aggregation layer (kubeadm's front-proxy flags): extension API servers such as metrics-server.
    --requestheader-client-ca-file="${PKI}/front-proxy-ca.crt" --requestheader-allowed-names=front-proxy-client
    --requestheader-username-headers=X-Remote-User --requestheader-group-headers=X-Remote-Group
    --requestheader-extra-headers-prefix=X-Remote-Extra-
    --proxy-client-cert-file="${PKI}/front-proxy-client.crt" --proxy-client-key-file="${PKI}/front-proxy-client.key"
    # TLS bootstrapping with `kubeadm token`s.
    --enable-bootstrap-token-auth
  )
fi

STORAGE=${STORAGE:-etcd}
STORAGE_FLAGS=(--data-dir="${DATA_DIR}")
if [[ "${STORAGE}" == "etcd" ]]; then
  ETCD_IMAGE=${ETCD_IMAGE:-registry.k8s.io/etcd:3.5.21-0}
  ETCD_PORT=${ETCD_PORT:-2379}
  ETCD_CONTAINER=rustkube-etcd-${NODE_NAME}
  mkdir -p "${OUT}/etcd" && chmod 700 "${OUT}/etcd"
  docker rm -f "${ETCD_CONTAINER}" >/dev/null 2>&1 || true
  if [[ "${SECURE}" == "1" ]]; then
    E=https
    ETCD_TLS=(
      --cert-file=/etc/kubernetes/pki/etcd/server.crt --key-file=/etc/kubernetes/pki/etcd/server.key
      --trusted-ca-file=/etc/kubernetes/pki/etcd/ca.crt --client-cert-auth=true
      --peer-cert-file=/etc/kubernetes/pki/etcd/peer.crt --peer-key-file=/etc/kubernetes/pki/etcd/peer.key
      --peer-trusted-ca-file=/etc/kubernetes/pki/etcd/ca.crt --peer-client-cert-auth=true
    )
    STORAGE_FLAGS=(--etcd-servers="https://127.0.0.1:${ETCD_PORT}" --etcd-cafile="${PKI}/etcd/ca.crt"
      --etcd-certfile="${PKI}/apiserver-etcd-client.crt" --etcd-keyfile="${PKI}/apiserver-etcd-client.key")
  else
    E=http
    ETCD_TLS=()
    STORAGE_FLAGS=(--etcd-servers="http://127.0.0.1:${ETCD_PORT}")
  fi
  echo "+++ Starting etcd (${ETCD_IMAGE}) on ${E}://127.0.0.1:${ETCD_PORT}, data in ${OUT}/etcd"
  # The flags kubeadm puts into the etcd static pod manifest.
  docker run -d --name "${ETCD_CONTAINER}" --network host --user "$(id -u):$(id -g)" \
    --label io.rustkubernetes.component=etcd \
    -v "${OUT}/etcd:/var/lib/etcd" -v "${PKI}/etcd:/etc/kubernetes/pki/etcd:ro" \
    "${ETCD_IMAGE}" etcd \
    --name="${NODE_NAME}" --data-dir=/var/lib/etcd \
    --advertise-client-urls="${E}://${API_HOST}:${ETCD_PORT}" \
    --listen-client-urls="${E}://127.0.0.1:${ETCD_PORT},${E}://${API_HOST}:${ETCD_PORT}" \
    --initial-advertise-peer-urls="${E}://${API_HOST}:$((ETCD_PORT + 1))" \
    --listen-peer-urls="${E}://${API_HOST}:$((ETCD_PORT + 1))" \
    --initial-cluster="${NODE_NAME}=${E}://${API_HOST}:$((ETCD_PORT + 1))" \
    --listen-metrics-urls="http://127.0.0.1:$((ETCD_PORT + 2))" \
    --experimental-initial-corrupt-check=true --experimental-watch-progress-notify-interval=5s \
    --snapshot-count=10000 "${ETCD_TLS[@]}" >/dev/null
  docker logs -f "${ETCD_CONTAINER}" > "${LOG_DIR}/etcd.log" 2>&1 &
  PIDS+=($!)
fi

if [[ "${ENCRYPT_SECRETS:-0}" == "1" && "${STORAGE}" == "etcd" ]]; then
  ENC=${OUT}/encryption-config.yaml
  if [[ ! -f "${ENC}" ]]; then
    (umask 077; cat > "${ENC}" <<ENCEOF
apiVersion: apiserver.config.k8s.io/v1
kind: EncryptionConfiguration
resources:
  - resources: [secrets]
    providers:
      - aesgcm:
          keys:
            - name: key1
              secret: $(head -c 32 /dev/urandom | base64)
      - identity: {}
ENCEOF
    )
  fi
  echo "+++ Secrets are encrypted at rest (${ENC})"
  STORAGE_FLAGS+=(--encryption-provider-config="${ENC}")
fi

echo "+++ Starting kube-apiserver on ${SCHEME}://${API_HOST}:${API_PORT}"
"${BIN}/kube-apiserver" \
  --bind-address="${API_HOST}" \
  --port="${API_PORT}" \
  --advertise-address="${API_HOST}" \
  "${STORAGE_FLAGS[@]}" \
  --service-cluster-ip-range="${SERVICE_CIDR}" \
  --service-node-port-range="${NODE_PORT_RANGE}" \
  "${SECURE_FLAGS[@]}" \
  --v="${VERBOSITY}" > "${LOG_DIR}/kube-apiserver.log" 2>&1 &
APISERVER_PID=$!
PIDS+=($!)

for _ in $(seq 1 100); do
  if curl -sfk "${SCHEME}://${API_HOST}:${API_PORT}/readyz" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "${APISERVER_PID}" 2>/dev/null; then
    echo "!!! kube-apiserver exited; see ${LOG_DIR}/kube-apiserver.log" >&2
    tail -20 "${LOG_DIR}/kube-apiserver.log" >&2
    exit 1
  fi
  sleep 0.2
done

echo "+++ Starting kube-controller-manager"
KCM_FLAGS=()
# Upstream local-up passes --cluster-signing-{cert,key}-file: the controller manager signs CSRs with the cluster CA.
[[ "${SECURE}" == "1" ]] && KCM_FLAGS=(--root-ca-file="${PKI}/ca.crt"
  --cluster-signing-cert-file="${PKI}/ca.crt" --cluster-signing-key-file="${PKI}/ca.key"
  --controllers='*,bootstrapsigner,tokencleaner')
# Upstream local-up: ENABLE_HOSTPATH_PROVISIONER + the `standard` default StorageClass.
ENABLE_HOSTPATH_PROVISIONER=${ENABLE_HOSTPATH_PROVISIONER:-true}
KCM_FLAGS+=(--enable-hostpath-provisioner="${ENABLE_HOSTPATH_PROVISIONER}")
if [[ "${NET_PLUGIN}" == "kubenet" ]]; then
  KCM_FLAGS+=(--allocate-node-cidrs=true --cluster-cidr="${POD_CIDR}" --service-cluster-ip-range="${SERVICE_CIDR}")
fi
"${BIN}/kube-controller-manager" \
  --kubeconfig="${KCM_CONF}" \
  --bind-address=127.0.0.1 \
  --port="${KCM_PORT:-10257}" \
  "${KCM_FLAGS[@]}" \
  --v="${VERBOSITY}" > "${LOG_DIR}/kube-controller-manager.log" 2>&1 &
PIDS+=($!)

echo "+++ Starting kube-scheduler"
"${BIN}/kube-scheduler" \
  --kubeconfig="${SCHED_CONF}" \
  --bind-address=127.0.0.1 \
  --port="${SCHED_PORT:-10259}" \
  --v="${VERBOSITY}" > "${LOG_DIR}/kube-scheduler.log" 2>&1 &
PIDS+=($!)

if [[ "${KUBELET}" == "1" ]]; then
  echo "+++ Starting kubelet (node ${NODE_NAME}, Docker runtime)"
  KUBELET_FLAGS=()
  if [[ "${SECURE}" == "1" ]]; then
    KUBELET_FLAGS=(
      --tls-cert-file="${PKI}/kubelet.crt" --tls-private-key-file="${PKI}/kubelet.key"
      --client-ca-file="${PKI}/ca.crt" --authorization-mode=Webhook
      --cluster-dns="${CLUSTER_DNS}" --cluster-domain="${CLUSTER_DOMAIN}"
      --rotate-certificates --cert-dir="${OUT}/kubelet-pki"
    )
  fi
  if [[ "${NET_PLUGIN}" == "kubenet" ]]; then
    KUBELET_FLAGS+=(--network-plugin=kubenet --kubenet-bridge-name="${KUBENET_BRIDGE}")
    [[ "${FLANNEL_BACKEND}" == "vxlan" ]] && KUBELET_FLAGS+=(--network-plugin-mtu=1450)
  fi
  "${BIN}/kubelet" \
    --kubeconfig="${KUBELET_CONF}" \
    --hostname-override="${NODE_NAME}" \
    --node-ip="${API_HOST}" \
    --port="${KUBELET_PORT:-10250}" \
    --root-dir="${OUT}/kubelet" \
    --container-runtime-endpoint="${DOCKER_HOST:-unix:///var/run/docker.sock}" \
    "${KUBELET_FLAGS[@]}" \
    --v="${VERBOSITY}" > "${LOG_DIR}/kubelet.log" 2>&1 &
  PIDS+=($!)
fi

KUBE_PROXY=${KUBE_PROXY:-${KUBELET}}
if [[ "${KUBE_PROXY}" == "1" ]]; then
  if ! docker image inspect "${PROXY_IMAGE}" >/dev/null 2>&1; then
    echo "+++ Building ${PROXY_IMAGE} (once)"
    docker build -q -t "${PROXY_IMAGE}" "${ROOT}/build/kube-proxy" >/dev/null
  fi
  if [[ "${NET_PLUGIN}" == "kubenet" ]]; then
    CLUSTER_CIDR=${POD_CIDR}
    HAIRPIN_BRIDGE=${KUBENET_BRIDGE}
  fi
  CLUSTER_CIDR=${CLUSTER_CIDR:-$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Subnet}}')}
  PROXY_CONTAINER=rustkube-proxy-${NODE_NAME}
  echo "+++ Starting kube-proxy (nftables, cluster CIDR ${CLUSTER_CIDR}) in container ${PROXY_CONTAINER}"
  docker rm -f "${PROXY_CONTAINER}" >/dev/null 2>&1 || true
  docker run -d --name "${PROXY_CONTAINER}" --network host --cap-add NET_ADMIN --cap-add NET_RAW \
    --label io.rustkubernetes.component=kube-proxy \
    -v "${BIN}/kube-proxy:/usr/local/bin/kube-proxy:ro" \
    -v "${PROXY_CONF}:/etc/kubernetes/kubeconfig:ro" \
    "${PROXY_IMAGE}" \
    --kubeconfig=/etc/kubernetes/kubeconfig \
    --hostname-override="${NODE_NAME}" \
    --cluster-cidr="${CLUSTER_CIDR}" \
    --nft-table="${PROXY_TABLE}" \
    --hairpin-bridge="${HAIRPIN_BRIDGE:-docker0}" \
    --healthz-bind-address="127.0.0.1:${PROXY_HEALTHZ_PORT:-10256}" \
    --metrics-bind-address="127.0.0.1:${PROXY_METRICS_PORT:-10249}" \
    --v="${VERBOSITY}" >/dev/null
  docker logs -f "${PROXY_CONTAINER}" > "${LOG_DIR}/kube-proxy.log" 2>&1 &
  PIDS+=($!)
fi

HOLLOW_NODES=${HOLLOW_NODES:-0}
if [[ "${HOLLOW_NODES}" -gt 0 ]]; then
  echo "+++ Starting ${HOLLOW_NODES} hollow nodes (kubemark)"
  "${BIN}/kubemark" \
    --kubeconfig="${ADMIN_CONF}" \
    --nodes="${HOLLOW_NODES}" \
    --cpu="${HOLLOW_CPU:-4}" \
    --memory="${HOLLOW_MEMORY:-8Gi}" \
    --zones="${HOLLOW_ZONES:-1}" \
    --v="${VERBOSITY}" > "${LOG_DIR}/kubemark.log" 2>&1 &
  PIDS+=($!)
fi

if [[ "${NET_PLUGIN}" == "kubenet" && "${KUBELET}" == "1" ]]; then
  echo "+++ Deploying the flannel pod network add-on (${FLANNEL_BACKEND}, ${POD_CIDR})"
  for _ in $(seq 1 50); do
    kubectl --kubeconfig "${ADMIN_CONF}" get ns kube-system >/dev/null 2>&1 && break
    sleep 0.2
  done
  sed -e "s#10.244.0.0/16#${POD_CIDR}#" -e "s#\"Type\": \"vxlan\"#\"Type\": \"${FLANNEL_BACKEND}\"#" "${ROOT}/cluster/addons/flannel/kube-flannel.yaml" |
    kubectl --kubeconfig "${ADMIN_CONF}" apply -f - 2>&1 | sed 's/^/    /' | cat
fi

DNS=${DNS:-$([[ "${SECURE}" == "1" && "${KUBELET}" == "1" && "${KUBE_PROXY}" == "1" ]] && echo 1 || echo 0)}
if [[ "${DNS}" == "1" ]]; then
  echo "+++ Deploying the CoreDNS add-on (kube-dns Service ${CLUSTER_DNS})"
  for _ in $(seq 1 50); do
    kubectl --kubeconfig "${ADMIN_CONF}" get ns kube-system >/dev/null 2>&1 && break
    sleep 0.2
  done
  sed -e "s/10\.96\.0\.10/${CLUSTER_DNS}/" -e "s/cluster\.local/${CLUSTER_DOMAIN}/" "${ROOT}/cluster/addons/dns/coredns/coredns.yaml" |
    kubectl --kubeconfig "${ADMIN_CONF}" apply -f - 2>&1 | sed 's/^/    /' | cat
fi

if [[ "${SECURE}" == "1" ]]; then
  echo "+++ Configuring bootstrap tokens and kubelet certificate rotation (kubeadm init phase bootstrap-token)"
  "${BIN}/kubeadm" init phase bootstrap-token --kubeconfig "${ADMIN_CONF}" 2>&1 | sed 's/^/    /' | cat
fi

if [[ "${ENABLE_HOSTPATH_PROVISIONER}" == "true" ]]; then
  echo "+++ Creating the default StorageClass (standard, kubernetes.io/host-path)"
  kubectl --kubeconfig "${ADMIN_CONF}" apply -f "${ROOT}/cluster/addons/storage-class/local/default.yaml" 2>&1 | sed 's/^/    /' | cat
fi

METRICS=${METRICS:-${DNS}}
if [[ "${METRICS}" == "1" ]]; then
  echo "+++ Deploying the metrics-server add-on (kubectl top)"
  kubectl --kubeconfig "${ADMIN_CONF}" apply -f "${ROOT}/cluster/addons/metrics-server/metrics-server.yaml" 2>&1 | sed 's/^/    /' | cat
fi

cat <<EOF

Local rustkubernetes cluster is running (${SCHEME}://${API_HOST}:${API_PORT}). Press Ctrl-C to shut it down.

Logs:
  ${LOG_DIR}/etcd.log
  ${LOG_DIR}/kube-apiserver.log
  ${LOG_DIR}/kube-controller-manager.log
  ${LOG_DIR}/kube-scheduler.log
  ${LOG_DIR}/kubelet.log
  ${LOG_DIR}/kube-proxy.log
  ${LOG_DIR}/kubemark.log

To start using your cluster, open another terminal and run:

  export KUBECONFIG=${KUBECONFIG_FILE}
  kubectl get nodes
  kubectl auth whoami
  kubectl run web --image=nginx:1.27-alpine && kubectl logs -f web
  kubectl exec -it web -- sh
  kubectl expose pod web --port=80 && kubectl run -it --rm client --image=busybox:1.36 -- wget -qO- web
  ${BIN}/kubeadm kubeconfig user --client-name jane --org dev --cert-dir ${PKI} --control-plane-endpoint https://${API_HOST}:${API_PORT} > jane.conf

EOF

wait "${APISERVER_PID}"
