#!/bin/sh
# rustkubernetes offline installer (part of the bundle built by rustkubeadmin).
#
#   sh install.sh PREFIX
#
# Installs the binaries into PREFIX/bin, loads the container images shipped in
# the bundle (no registry access needed), picks a process supervisor and prints
# machine facts as "FACT key=value" lines for rustkubeadmin.
set -eu
PREFIX=${1:?usage: install.sh PREFIX}
case "$PREFIX" in "~/"*) PREFIX="$HOME/${PREFIX#\~/}" ;; esac
HERE=$(cd "$(dirname "$0")" && pwd)
say() { echo "[install] $*"; }
fail() { echo "[install] ERROR: $*" >&2; exit 1; }

say "rustkubernetes $(cat "$HERE/VERSION") → $PREFIX (offline bundle, no downloads)"
[ "$(uname -s)" = Linux ] || fail "Linux is required"
ARCH=$(uname -m)
[ "$ARCH" = "$(cat "$HERE/ARCH")" ] || fail "the bundle is for $(cat "$HERE/ARCH") but this machine is $ARCH"
command -v docker >/dev/null 2>&1 || fail "Docker is required (the kubelet runs pods with Docker): install docker.io and add $(id -un) to the docker group"
docker info >/dev/null 2>&1 || fail "cannot talk to the Docker daemon as $(id -un) (is it running, and is the user in the docker group?)"

mkdir -p "$PREFIX/bin" "$PREFIX/clusters"
for f in "$HERE"/bin/*; do
  cp -f "$f" "$PREFIX/bin/.tmp-$$" && chmod 0755 "$PREFIX/bin/.tmp-$$" && mv -f "$PREFIX/bin/.tmp-$$" "$PREFIX/bin/$(basename "$f")"
done
say "installed: $(ls "$HERE/bin" | tr '\n' ' ')"
for t in "$HERE"/images/*.tar; do
  [ -f "$t" ] || continue
  say "loading image $(basename "$t" .tar)"
  docker load -q -i "$t" >/dev/null
done
cp "$HERE/VERSION" "$PREFIX/VERSION"

if [ "$(id -u)" = 0 ] && command -v systemctl >/dev/null 2>&1 && systemctl show-environment >/dev/null 2>&1; then
  SUP=systemd
elif command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
  SUP=systemd-user
else
  SUP=nohup
fi
echo "$SUP" > "$PREFIX/SUPERVISOR"
say "process supervisor: $SUP"
BR=$(cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>/dev/null || echo 0)
[ "$BR" = 1 ] || say "warning: br_netfilter is not active (pod-to-service traffic on the same node may fail)"

. /etc/os-release 2>/dev/null || PRETTY_NAME=Linux
echo "FACT hostname=$(hostname)"
echo "FACT os=$PRETTY_NAME"
echo "FACT kernel=$(uname -r)"
echo "FACT arch=$ARCH"
echo "FACT cpus=$(nproc 2>/dev/null || echo 1)"
echo "FACT memory_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo)"
echo "FACT docker=$(docker version -f '{{.Server.Version}}' 2>/dev/null || echo unknown)"
echo "FACT supervisor=$SUP"
echo "FACT prefix=$PREFIX"
echo "FACT user=$(id -un)"
echo "FACT br_netfilter=$BR"
echo "FACT version=$(cat "$HERE/VERSION")"
say "done"
