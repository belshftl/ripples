#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

docker build -t ripples-build scripts/build
mkdir -p dist
docker run --rm \
    -e OWNER="$(id -u):$(id -g)" \
    -v "$PWD:/src:ro" \
    -v "$PWD/dist:/dist" \
    -v ripples-build-registry:/usr/local/cargo/registry \
    -v ripples-build-target:/target \
    ripples-build sh /src/scripts/build/artifacts.sh
