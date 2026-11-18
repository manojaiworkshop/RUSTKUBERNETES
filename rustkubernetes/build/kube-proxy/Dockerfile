# Runtime image for kube-proxy (like kubernetes/build/server-image/kube-proxy):
# provides nft, conntrack and bridge (iproute2, for hairpin mode). The kube-proxy binary is mounted at run time
# (hack/local-up-cluster.sh), so rebuilding Rust code needs no image rebuild.
#
#   docker build -t rustkubernetes/kube-proxy-base:24.04 build/kube-proxy
#
# Ubuntu 24.04 matches the glibc the binaries are built against on the dev host.
FROM ubuntu:24.04
RUN apt-get update \
 && apt-get install -y --no-install-recommends nftables conntrack iproute2 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["/usr/local/bin/kube-proxy"]
