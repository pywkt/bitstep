#!/bin/sh
# Builds target/bundled/bitstep.clap inside an Ubuntu 24.04 container, so it
# loads in the Flatpak version of Bitwig and on systems with an older glibc.
#
#   ./scripts/build-in-docker.sh          (or with sudo if your user is not
#                                           in the docker group)
set -eu

cd "$(dirname "$0")/.."

# The files the build writes are handed back to you, even when run with sudo.
owner="${SUDO_UID:-$(id -u)}:${SUDO_GID:-$(id -g)}"

docker build -t bitstep-build docker

# Downloaded crates are kept in a named volume, so later builds are faster.
docker run --rm \
    -v "$PWD":/src \
    -v bitstep-cargo-registry:/usr/local/cargo/registry \
    bitstep-build \
    sh -c "cargo xtask bundle bitstep --release; status=\$?; chown -R $owner target; exit \$status"

echo
echo "Built target/bundled/bitstep.clap"
