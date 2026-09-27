#!/bin/sh
# Build both tcpool images locally with podman. Builds only; nothing is pushed (CI does that, see
# .github/workflows/tcpool-images.yml).
#
#   transcode/deploy/build-images.sh [tag]     # default tag: the short git sha, else "dev"
#
# The Containerfiles COPY prebuilt binaries from the build context, so this stages a tiny context
# (three binaries plus tcpool-entry) rather than handing podman the whole workspace with target/ in
# it. Binaries come from deploy/build-musl.sh if they are not there yet.
set -eu

DEPLOY=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
WS=$(CDPATH='' cd -- "$DEPLOY/.." && pwd)
REL="$WS/target/x86_64-unknown-linux-musl/release"
REGISTRY=${TC_REGISTRY:-ghcr.io/saabstory404}
TAG=${1:-$(git -C "$WS" rev-parse --short HEAD 2>/dev/null || echo dev)}

if [ ! -x "$REL/tcpool-agent" ]; then
    echo "no static binaries yet; running build-musl.sh" >&2
    "$DEPLOY/build-musl.sh"
fi

for b in tcpool-agent tcpool-shim tcpool-sync; do
    case $(file -b "$REL/$b") in
        *static-pie*|*statically\ linked*) ;;
        *) echo "$b is not statically linked: $(file -b "$REL/$b")" >&2; exit 1 ;;
    esac
done

CTX=$(mktemp -d)
trap 'rm -rf "$CTX"' EXIT INT TERM
cp "$REL/tcpool-agent" "$REL/tcpool-shim" "$REL/tcpool-sync" "$DEPLOY/tcpool-entry" "$CTX/"
chmod 0755 "$CTX"/*

podman build -t "$REGISTRY/tcpool-agent:$TAG" -f "$DEPLOY/Containerfile.agent" "$CTX"
podman build -t "$REGISTRY/tcpool-shim:$TAG" -f "$DEPLOY/Containerfile.shim" "$CTX"

podman image inspect --format '{{.Id}} {{index .RepoTags 0}} {{.Size}}' \
    "$REGISTRY/tcpool-agent:$TAG" "$REGISTRY/tcpool-shim:$TAG"
