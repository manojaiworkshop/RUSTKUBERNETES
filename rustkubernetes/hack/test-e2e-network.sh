#!/usr/bin/env bash
# Copyright 2026 The rustkubernetes Authors.
# Licensed under the Apache License, Version 2.0.
#
# Multi-node pod network tests. Two "machines" are privileged containers of build/e2e-node (each with its own
# Docker Engine, network namespace and kernel routing table, like kind's nodes) on a private Docker network:
#
#   n1: kube-apiserver, kube-controller-manager (--allocate-node-cidrs), kube-scheduler, kubelet, kube-proxy
#   n2: kubelet, kube-proxy
#
# The kubelets use --network-plugin=kubenet (bridge cbr0 on the node's spec.podCIDR) and the flannel add-on
# (cluster/addons/flannel) routes between the nodes. Checks: node IPAM, NetworkReady/NotReady, pod-to-pod across
# nodes without NAT, Services and NodePorts with remote endpoints, DNS from both nodes, masquerading of traffic
# leaving the cluster, validation of spec.podCIDR.
#
# Nothing runs in the host's network namespace, so this is safe next to other clusters on the same machine.
#
# Requires: docker (user in the docker group), kubectl, and network access to pull docker:29-dind,
# ubuntu:24.04, ghcr.io/flannel-io/flannel:v0.27.4, busybox:1.36, registry.k8s.io/pause:3.10 and
# registry.k8s.io/coredns/coredns:v1.12.1 the first time.
#
# Usage: hack/test-e2e-network.sh            (BACKEND=host-gw to test flannel's host-gw backend instead of vxlan)

set -o errexit -o nounset -o pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "${ROOT}"

command -v docker >/dev/null || { echo "docker is required" >&2; exit 1; }
command -v kubectl >/dev/null || { echo "kubectl is required" >&2; exit 1; }
docker version >/dev/null 2>&1 || { echo "cannot talk to Docker (is your user in the docker group?)" >&2; exit 1; }

cargo build -q -p kube-apiserver -p kube-controller-manager -p kube-scheduler -p kubelet -p kube-proxy -p kubeadm
hack/build-portforward-helper.sh
B=${ROOT}/target/debug

BACKEND=${BACKEND:-vxlan}
CLUSTER_CIDR=10.244.0.0/16
SERVICE_CIDR=10.96.0.0/12
if [[ "${BACKEND}" == "vxlan" ]]; then MTU=1450; else MTU=1500; fi
NODE_IMAGE=rustkubernetes/e2e-node:24.04
FLANNEL_IMAGE=ghcr.io/flannel-io/flannel:v0.27.4
METRICS_IMAGE=registry.k8s.io/metrics-server/metrics-server:v0.8.0
LPP_IMAGE=rancher/local-path-provisioner:v0.0.32
IMAGES=(registry.k8s.io/pause:3.10 busybox:1.36 busybox:latest "${FLANNEL_IMAGE}" registry.k8s.io/coredns/coredns:v1.12.1 "${METRICS_IMAGE}" "${LPP_IMAGE}")
ID=${ID:-$$}
NET=rk-e2e-net-${ID}
NODES=(n1 n2)
WORK=${ROOT}/_output/test-e2e-network
RUN=${WORK}/run-${ID}
NS=e2e-net

cleanup() {
  trap - EXIT
  if [[ "${KEEP:-0}" == "1" ]]; then
    echo "KEEP=1: machines rk-${ID}-n1/n2 left running (KUBECONFIG=${RUN}/admin.conf); remove with: ID=${ID} hack/test-e2e-network.sh --cleanup"
    return
  fi
  for n in "${NODES[@]}"; do
    mkdir -p "${WORK}/logs-${ID}/${n}"
    docker cp "rk-${ID}-${n}:/var/log/rk/." "${WORK}/logs-${ID}/${n}/" >/dev/null 2>&1 || true
    docker rm -fv "rk-${ID}-${n}" >/dev/null 2>&1 || true
  done
  docker rm -f "rk-${ID}-outside" >/dev/null 2>&1 || true
  docker network rm "${NET}" >/dev/null 2>&1 || true
}
trap cleanup EXIT
if [[ "${1:-}" == "--cleanup" ]]; then KEEP=0; exit 0; fi

# --- machines ------------------------------------------------------------------------------------------------
docker image inspect "${NODE_IMAGE}" >/dev/null 2>&1 || docker build -q -t "${NODE_IMAGE}" build/e2e-node >/dev/null
for img in "${IMAGES[@]}"; do docker image inspect "${img}" >/dev/null 2>&1 || docker pull -q "${img}" >/dev/null; done
mkdir -p "${RUN}"
[[ -s "${WORK}/images.tar" && "${WORK}/images.tar" -nt "${BASH_SOURCE[0]}" ]] || docker save "${IMAGES[@]}" -o "${WORK}/images.tar"

# A /24 for the machines that no Docker network uses yet.
SUBNET=
for i in $(seq 201 250); do
  if docker network create --subnet "10.${i}.0.0/24" "${NET}" >/dev/null 2>&1; then SUBNET=10.${i}.0; break; fi
done
[[ -n "${SUBNET}" ]] || { echo "no free subnet for the test network" >&2; exit 1; }
declare -A IP=([n1]=${SUBNET}.11 [n2]=${SUBNET}.12)
OUTSIDE_IP=${SUBNET}.50

for n in "${NODES[@]}"; do
  docker run -d --privileged --name "rk-${ID}-${n}" --hostname "${n}" --network "${NET}" --ip "${IP[$n]}" \
    -v "${B}:/rk/bin:ro" -v "${RUN}:/etc/kubernetes:ro" "${NODE_IMAGE}" >/dev/null
done
for n in "${NODES[@]}"; do
  for _ in $(seq 1 60); do docker exec "rk-${ID}-${n}" docker info >/dev/null 2>&1 && break; sleep 1; done
  docker exec -i "rk-${ID}-${n}" docker load < "${WORK}/images.tar" >/dev/null
  docker exec "rk-${ID}-${n}" mkdir -p /var/log/rk
done
# A host outside the cluster (for egress / masquerade checks): prints the client address it sees.
docker run -d --name "rk-${ID}-outside" --network "${NET}" --ip "${OUTSIDE_IP}" busybox:1.36 sh -c \
  'mkdir -p /www/cgi-bin; printf "#!/bin/sh\necho Content-Type: text/plain\necho\necho client=\$REMOTE_ADDR\n" > /www/cgi-bin/who; chmod +x /www/cgi-bin/who; httpd -f -p 8080 -h /www' >/dev/null

# --- PKI (kubeadm) -------------------------------------------------------------------------------------------
"${B}/kubeadm" init --cert-dir "${RUN}/pki" --kubeconfig-dir "${RUN}" --apiserver-advertise-address "${IP[n1]}" \
  --node-name n1 --node-ip "${IP[n1]}" >/dev/null
"${B}/kubeadm" node-credentials --node-name n2 --node-ip "${IP[n2]}" --out "${RUN}/n2" --cert-dir "${RUN}/pki" \
  --apiserver-advertise-address "${IP[n1]}" >/dev/null
mkdir -p "${RUN}/n1" && cp "${RUN}/pki/kubelet.crt" "${RUN}/pki/kubelet.key" "${RUN}/kubelet.conf" "${RUN}/n1/"
export KUBECONFIG=${RUN}/admin.conf
P=/etc/kubernetes/pki

# start <node> <name> <command...>: runs a component in the background inside a machine.
start() {
  local n=$1 name=$2
  shift 2
  docker exec -d "rk-${ID}-${n}" sh -c "exec $* > /var/log/rk/${name}.log 2>&1"
}

start n1 kube-apiserver /rk/bin/kube-apiserver --port 6443 --bind-address "${IP[n1]}" --advertise-address "${IP[n1]}" \
  --service-cluster-ip-range "${SERVICE_CIDR}" \
  --tls-cert-file ${P}/apiserver.crt --tls-private-key-file ${P}/apiserver.key --client-ca-file ${P}/ca.crt \
  --authorization-mode Node,RBAC --enable-admission-plugins NodeRestriction --service-account-key-file ${P}/sa.pub --service-account-signing-key-file ${P}/sa.key \
  --kubelet-client-certificate ${P}/apiserver-kubelet-client.crt --kubelet-client-key ${P}/apiserver-kubelet-client.key \
  --kubelet-certificate-authority ${P}/ca.crt \
  --requestheader-client-ca-file ${P}/front-proxy-ca.crt --requestheader-allowed-names front-proxy-client \
  --requestheader-username-headers X-Remote-User --requestheader-group-headers X-Remote-Group \
  --requestheader-extra-headers-prefix X-Remote-Extra- \
  --proxy-client-cert-file ${P}/front-proxy-client.crt --proxy-client-key-file ${P}/front-proxy-client.key
for _ in $(seq 1 100); do curl -sfk "https://${IP[n1]}:6443/readyz" >/dev/null 2>&1 && break; sleep 0.2; done
start n1 kube-scheduler /rk/bin/kube-scheduler --kubeconfig /etc/kubernetes/scheduler.conf --leader-elect=false
for n in "${NODES[@]}"; do
  start "${n}" kubelet /rk/bin/kubelet --kubeconfig "/etc/kubernetes/${n}/kubelet.conf" --hostname-override "${n}" \
    --node-ip "${IP[$n]}" --root-dir /var/lib/kubelet --tls-cert-file "/etc/kubernetes/${n}/kubelet.crt" \
    --tls-private-key-file "/etc/kubernetes/${n}/kubelet.key" --client-ca-file ${P}/ca.crt --authorization-mode Webhook \
    --cluster-dns 10.96.0.10 --network-plugin kubenet --network-plugin-mtu "${MTU}"
  start "${n}" kube-proxy /rk/bin/kube-proxy --kubeconfig /etc/kubernetes/kube-proxy.conf --hostname-override "${n}" \
    --cluster-cidr "${CLUSTER_CIDR}" --hairpin-bridge cbr0
done

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
on() { docker exec "rk-${ID}-$1" "${@:2}"; }

echo "+++ machines n1=${IP[n1]} n2=${IP[n2]} (backend ${BACKEND}, logs in ${WORK}/logs-${ID})"
echo "+++ kubenet without a pod CIDR (no node IPAM yet)"
for _ in $(seq 1 60); do [[ $(kubectl get nodes --no-headers 2>/dev/null | wc -l) -ge 2 ]] && break; sleep 1; done
run "nodes registered"             "n2"                kubectl get nodes
run "NotReady: no pod CIDR"        "kubenet does not have netConfig. This is most likely due to lack of PodCIDR" \
  bash -c "sleep 2; kubectl get node n1 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].message}'"
run "network error format"         "container runtime network not ready: NetworkReady=false reason:NetworkPluginNotReady message:docker: network plugin is not ready" \
  kubectl get node n2 -o jsonpath='{.status.conditions[?(@.type=="Ready")].message}'

echo "+++ node IPAM (kube-controller-manager --allocate-node-cidrs)"
start n1 kube-controller-manager /rk/bin/kube-controller-manager --kubeconfig /etc/kubernetes/controller-manager.conf \
  --root-ca-file ${P}/ca.crt --leader-elect=false --allocate-node-cidrs=true --cluster-cidr "${CLUSTER_CIDR}" \
  --service-cluster-ip-range "${SERVICE_CIDR}" --node-cidr-mask-size 24
run "nodes become Ready"           "condition met"     kubectl wait --for=condition=Ready node/n1 node/n2 --timeout=90s
C1=$(kubectl get node n1 -o jsonpath='{.spec.podCIDR}' | cat)
C2=$(kubectl get node n2 -o jsonpath='{.spec.podCIDR}' | cat)
run "n1 got a /24 of the cluster CIDR" "10.244."       echo "${C1}"
run "pod CIDRs differ"             "differ"            bash -c "[[ -n '${C1}' && -n '${C2}' && '${C1}' != '${C2}' ]] && echo differ || echo '${C1} ${C2}'"
run "podCIDRs mirrors podCIDR"     "[\"${C1}\"]"       kubectl get node n1 -o jsonpath='{.spec.podCIDRs}'
run "podCIDR is immutable"         "may not change podCIDR" kubectl patch node n1 -p '{"spec":{"podCIDR":"10.244.200.0/24","podCIDRs":["10.244.200.0/24"]}}'
run "cbr0 on the pod CIDR"         "${C1%.0/24}.1/24"  on n1 ip -4 addr show cbr0
run "cbr0 MTU"                     "mtu ${MTU}"        on n2 ip link show cbr0
run "kubelet log: Updating Pod CIDR" "Updating Pod CIDR" on n2 cat /var/log/rk/kubelet.log

echo "+++ pod network add-on (flannel, ${BACKEND})"
sed -e "s#10.244.0.0/16#${CLUSTER_CIDR}#" -e "s#\"Type\": \"vxlan\"#\"Type\": \"${BACKEND}\"#" cluster/addons/flannel/kube-flannel.yaml | kubectl apply -f - >/dev/null
run "flannel on every node"        "successfully rolled out" kubectl rollout status ds/kube-flannel-ds -n kube-flannel --timeout=120s
run "NetworkUnavailable=False"     "FlannelIsUp"       bash -c "sleep 3; kubectl get node n2 -o jsonpath='{.status.conditions[?(@.type==\"NetworkUnavailable\")].reason}'"
run "flannel annotations kept"     "flannel.alpha.coreos.com/public-ip" kubectl get node n1 -o jsonpath='{.metadata.annotations}'
if [[ "${BACKEND}" == "vxlan" ]]; then
  run "route to n2's pods (vxlan)" "${C2} via ${C2%/24} dev flannel.1" on n1 ip route
else
  run "route to n2's pods (host-gw)" "${C2} via ${IP[n2]}" on n1 ip route
fi

echo "+++ pods across nodes"
kubectl create ns "${NS}" >/dev/null
for n in "${NODES[@]}"; do
  cat <<EOF | ${K} apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: who-${n}, labels: {app: who}}
spec:
  nodeName: ${n}
  containers:
  - name: who
    image: busybox:1.36
    command: [sh, -c, 'mkdir -p /www/cgi-bin; printf "#!/bin/sh\necho Content-Type: text/plain\necho\necho client=\\\$REMOTE_ADDR server=\\\$(hostname)\n" > /www/cgi-bin/who; chmod +x /www/cgi-bin/who; httpd -f -p 8080 -h /www']
    ports: [{containerPort: 8080}]
EOF
done
run "pods Ready on both nodes"     "condition met"     ${K} wait --for=condition=Ready pod/who-n1 pod/who-n2 --timeout=90s
P1=$(${K} get pod who-n1 -o jsonpath='{.status.podIP}' | cat)
P2=$(${K} get pod who-n2 -o jsonpath='{.status.podIP}' | cat)
run "n1 pod IP in n1's CIDR"       "${C1%.0/24}."      echo "${P1}"
run "n2 pod IP in n2's CIDR"       "${C2%.0/24}."      echo "${P2}"
run "ping n1 pod -> n2 pod"        " 0% packet loss"   ${K} exec who-n1 -- ping -c 3 -W 2 "${P2}"
run "TCP n2 pod -> n1 pod, no NAT" "ffff:${P2}] server=who-n1" ${K} exec who-n2 -- wget -qO- -T 3 "http://${P1}:8080/cgi-bin/who"
run "node n1 -> pod on n2"         "server=who-n2"     on n1 bash -c "exec 3<>/dev/tcp/${P2}/8080; printf 'GET /cgi-bin/who HTTP/1.0\r\n\r\n' >&3; cat <&3"
run "pod resolv.conf (cluster DNS)" "nameserver 10.96.0.10" ${K} exec who-n2 -- cat /etc/resolv.conf
run "large packets (MTU)"          "client="           ${K} exec who-n1 -- sh -c "head -c 200000 /dev/zero > /www/big; wget -qO- -T 5 http://${P2}:8080/cgi-bin/who"
run "large transfer across nodes"  "200000"            ${K} exec who-n2 -- sh -c "wget -qO- -T 10 http://${P1}:8080/big | wc -c"

echo "+++ Services with endpoints on both nodes"
${K} expose pod who-n1 --name who --port 80 --target-port 8080 --selector app=who --type NodePort >/dev/null
sleep 3
CIP=$(${K} get svc who -o jsonpath='{.spec.clusterIP}' | cat)
NP=$(${K} get svc who -o jsonpath='{.spec.ports[0].nodePort}' | cat)
run "EndpointSlice has both pods"  "2"                 bash -c "${K} get endpointslices -l kubernetes.io/service-name=who -o jsonpath='{.items[0].endpoints[*].addresses[0]}' | wc -w"
run "ClusterIP reaches both nodes" "2"                 bash -c "for i in \$(seq 30); do ${K} exec who-n1 -- wget -qO- -T 2 http://${CIP}/cgi-bin/who; done | grep -o 'server=who-n[12]' | sort -u | wc -l"
run "ClusterIP keeps pod source"   "ffff:${P1}]"      bash -c "for i in \$(seq 10); do ${K} exec who-n1 -- wget -qO- -T 2 http://${CIP}/cgi-bin/who; done"
run "NodePort on n2 -> pod on n1"  "server=who-n1"     bash -c "for i in \$(seq 30); do curl -s -m 2 http://${IP[n2]}:${NP}/cgi-bin/who; done"

echo "+++ port-forward to a pod on the other node (kubectl -> API server on n1 -> kubelet on n2)"
PF=$((20000 + RANDOM % 10000))
${K} port-forward pod/who-n2 "${PF}:8080" > >(cat > "${WORK}/pf-${ID}.log") 2>&1 &
PF_PID=$!
sleep 3
run "port-forward across nodes"     "server=who-n2"     curl -s -m 5 "http://127.0.0.1:${PF}/cgi-bin/who"
run "port-forward: client is loopback" "ffff:127.0.0.1]"   curl -s -m 5 "http://127.0.0.1:${PF}/cgi-bin/who"
kill "${PF_PID}" 2>/dev/null || true

echo "+++ cluster DNS from both nodes"
kubectl apply -f cluster/addons/dns/coredns/coredns.yaml >/dev/null
run "CoreDNS ready"                "successfully rolled out" kubectl rollout status deploy/coredns -n kube-system --timeout=120s
run "DNS from n1"                  "Address"           bash -c "for i in 1 2 3 4 5; do out=\$(${K} exec who-n1 -- nslookup -timeout=3 who.${NS}.svc.cluster.local 2>&1 | cat); echo \"\$out\" | grep -q 'Address: 10.96' && break; sleep 2; done; echo \"\$out\""
run "DNS from n2"                  "Address"           bash -c "for i in 1 2 3 4 5; do out=\$(${K} exec who-n2 -- nslookup -timeout=3 who.${NS}.svc.cluster.local 2>&1 | cat); echo \"\$out\" | grep -q 'Address: 10.96' && break; sleep 2; done; echo \"\$out\""
run "Service by name"              "server=who-n"      ${K} exec who-n2 -- wget -qO- -T 3 http://who/cgi-bin/who
# (TCP only: busybox's TLS client cannot negotiate with the API server)
run "pods reach the API Service"   "443) open"         ${K} exec who-n2 -- nc -zv -w3 kubernetes.default.svc.cluster.local 443

echo "+++ resource metrics (kubelet /metrics/resource, metrics-server, aggregation layer, kubectl top)"
KC="--cacert ${RUN}/pki/ca.crt --cert ${RUN}/pki/apiserver-kubelet-client.crt --key ${RUN}/pki/apiserver-kubelet-client.key"
run "kubelet /metrics/resource"       "node_cpu_usage_seconds_total"   bash -c "curl -s ${KC} https://${IP[n2]}:10250/metrics/resource"
run "container metrics have labels"   'container_memory_working_set_bytes{container="who",namespace="e2e-net",pod="who-n2"}' bash -c "curl -s ${KC} https://${IP[n2]}:10250/metrics/resource"
run "kubelet metrics need auth"       "401"                            curl -sk -o /dev/null -w "%{http_code}" "https://${IP[n2]}:10250/metrics/resource"
run "built-in APIServices (Local)"    "Local"                          kubectl get apiservice v1.apps
run "authentication ConfigMap"        "front-proxy-client"             kubectl get cm extension-apiserver-authentication -n kube-system -o jsonpath='{.data.requestheader-allowed-names}'
kubectl apply -f cluster/addons/metrics-server/metrics-server.yaml >/dev/null
run "metrics-server ready"            "successfully rolled out"        kubectl rollout status deploy/metrics-server -n kube-system --timeout=180s
run "APIService available"            "condition met"                  kubectl wait --for=condition=Available apiservice/v1beta1.metrics.k8s.io --timeout=120s
run "discovery lists metrics.k8s.io"  "metrics.k8s.io/v1beta1"         kubectl api-versions
run "api-resources from the extension" "PodMetrics"                    kubectl api-resources --api-group=metrics.k8s.io -o wide
run "kubectl top nodes"               "n2"                             bash -c "for i in \$(seq 30); do out=\$(kubectl top nodes 2>&1 | cat); echo \"\$out\" | grep -q '^n2 ' && break; sleep 3; done; echo \"\$out\""
run "kubectl top pods"                "who-n1"                         bash -c "for i in \$(seq 30); do out=\$(kubectl top pods -n ${NS} 2>&1 | cat); echo \"\$out\" | grep -q who-n1 && break; sleep 3; done; echo \"\$out\""
run "kubectl top pod --containers"    "who"                            kubectl top pod who-n2 -n "${NS}" --containers
run "NodeMetrics via the API"         '"kind":"NodeMetricsList"'       kubectl get --raw /apis/metrics.k8s.io/v1beta1/nodes
run "RBAC before proxying"            "cannot list resource \"pods\" in API group \"metrics.k8s.io\"" kubectl get --raw "/apis/metrics.k8s.io/v1beta1/namespaces/${NS}/pods" --as="system:serviceaccount:${NS}:default"
kubectl create rolebinding sa-view --clusterrole=view --serviceaccount="${NS}:default" -n "${NS}" >/dev/null
run "aggregated view role (user via X-Remote-User)" '"kind":"PodMetricsList"' bash -c "sleep 2; kubectl get --raw /apis/metrics.k8s.io/v1beta1/namespaces/${NS}/pods --as=system:serviceaccount:${NS}:default"
kubectl scale deploy/metrics-server -n kube-system --replicas=0 >/dev/null
run "APIService unavailable"          "False (MissingEndpoints)"       bash -c "for i in \$(seq 20); do out=\$(kubectl get apiservice v1beta1.metrics.k8s.io 2>&1 | cat); echo \"\$out\" | grep -q MissingEndpoints && break; sleep 2; done; echo \"\$out\""
run "503 while unavailable"           "the server is currently unable to handle the request" kubectl get --raw /apis/metrics.k8s.io/v1beta1/nodes
kubectl scale deploy/metrics-server -n kube-system --replicas=1 >/dev/null

echo "+++ dynamic node-local volumes (external provisioner, WaitForFirstConsumer)"
kubectl apply -f cluster/addons/local-path-storage/local-path-storage.yaml >/dev/null
kubectl patch storageclass local-path -p '{"metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}}}' >/dev/null
run "local-path-provisioner ready"     "successfully rolled out"   kubectl rollout status deploy/local-path-provisioner -n local-path-storage --timeout=120s
run "default class"                    "local-path (default)"      kubectl get sc
cat <<EOF | ${K} apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: data}
spec:
  accessModes: [ReadWriteOnce]
  resources: {requests: {storage: 128Mi}}
---
apiVersion: v1
kind: Pod
metadata: {name: volume-user}
spec:
  nodeSelector: {kubernetes.io/hostname: n2}
  terminationGracePeriodSeconds: 1
  containers:
  - {name: app, image: busybox:1.36, command: [sh, -c, "echo on-n2 > /data/f; sleep 3600"], volumeMounts: [{name: data, mountPath: /data}]}
  volumes:
  - {name: data, persistentVolumeClaim: {claimName: data}}
EOF
run "pod with provisioned volume Ready" "condition met"            ${K} wait --for=condition=Ready pod/volume-user --timeout=180s
run "scheduler selected the node"      "n2"                        ${K} get pvc data -o jsonpath='{.metadata.annotations.volume\.kubernetes\.io/selected-node}'
run "claim handed to the provisioner"  "rancher.io/local-path"     ${K} get pvc data -o jsonpath='{.metadata.annotations.volume\.kubernetes\.io/storage-provisioner}'
run "ExternalProvisioning event"       "Waiting for a volume to be created either by the external provisioner 'rancher.io/local-path'" ${K} get events --field-selector involvedObject.name=data
LPV=$(${K} get pvc data -o jsonpath='{.spec.volumeName}' | cat)
run "PV pinned to n2"                  "n2"                        kubectl get pv "${LPV}" -o jsonpath='{.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].values[0]}'
run "data on n2's disk"                "on-n2"                     bash -c "docker exec rk-${ID}-n2 sh -c 'cat /opt/local-path-provisioner/${LPV}*/f'"
${K} delete pod volume-user --timeout=60s >/dev/null 2>&1 || true
${K} delete pvc data --timeout=60s >/dev/null 2>&1 || true
run "Delete reclaim by the provisioner" "NotFound"                 bash -c "for i in \$(seq 60); do kubectl get pv ${LPV} >/dev/null 2>&1 || break; sleep 2; done; kubectl get pv ${LPV} 2>&1"
run "directory removed on n2"          "0"                         bash -c "docker exec rk-${ID}-n2 sh -c 'ls -d /opt/local-path-provisioner/${LPV}* 2>/dev/null | wc -l'"

echo "+++ traffic leaving the cluster (flannel --ip-masq)"
run "egress masqueraded to node IP" "ffff:${IP[n2]}]" ${K} exec who-n2 -- wget -qO- -T 3 "http://${OUTSIDE_IP}:8080/cgi-bin/who"
run "hostNetwork pod uses node IP" "${IP[n2]}"         bash -c "${K} run hostnet --image=busybox:1.36 --overrides='{\"spec\":{\"hostNetwork\":true,\"nodeName\":\"n2\"}}' --command -- sleep 3600 >/dev/null; ${K} wait --for=condition=Ready pod/hostnet --timeout=60s >/dev/null; ${K} get pod hostnet -o jsonpath='{.status.podIP}'"

echo "+++ controller logs"
run "kcm log: Set node PodCIDR"    "Set node PodCIDR"  on n1 cat /var/log/rk/kube-controller-manager.log

echo
echo "${PASS} passed, ${FAIL} failed"
[[ ${FAIL} -eq 0 ]]
