# A "machine" for multi-node tests (hack/test-e2e-network.sh), like kind's node image: Ubuntu 24.04 (the glibc
# the binaries are built against) with its own Docker Engine, iptables/nftables, conntrack and iproute2. Run it
# privileged; the rustkubernetes binaries are mounted in at run time, so code changes need no rebuild.
#
#   docker build -t rustkubernetes/e2e-node:24.04 build/e2e-node
FROM docker:29-dind AS dind

FROM ubuntu:24.04
RUN apt-get update \
 && apt-get install -y --no-install-recommends iptables nftables conntrack iproute2 ca-certificates procps kmod iputils-ping \
 && rm -rf /var/lib/apt/lists/*
# Static Docker Engine binaries (dockerd, containerd, runc, docker CLI) and the dind cgroup helper.
COPY --from=dind /usr/local/bin/ /usr/local/bin/
VOLUME /var/lib/docker
ENTRYPOINT ["dind", "dockerd", "--host=unix:///var/run/docker.sock"]
