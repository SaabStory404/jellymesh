#!/bin/sh
# Build the three static musl binaries the images COPY in, using the same toolchain CI uses.
#
# The images take prebuilt binaries from the build context rather than compiling inside the
# Containerfile: the agent image's base is the Jellyfin image (no Rust), and the shim image is
# FROM scratch. CI builds them with cargo and then feeds them to `podman build`; this script is
# the local equivalent, so a developer can reproduce the image without a host musl toolchain.
#
#   transcode/deploy/build-musl.sh            # in a rust container (no host toolchain needed)
#   TC_BUILD=host transcode/deploy/build-musl.sh   # with the host's cargo + musl-gcc
#
# `ring` (rustls' crypto backend) compiles C, so the musl target needs a musl C compiler:
# musl-tools on Debian/Ubuntu, musl-gcc on Fedora. Without it the build fails in ring's build.rs.
# Output: transcode/target/x86_64-unknown-linux-musl/release/tcpool-{agent,shim,sync}
set -eu

TARGET=x86_64-unknown-linux-musl
WS=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
BUILDER_IMAGE=${TC_BUILDER_IMAGE:-docker.io/library/rust:1-bookworm}

if [ "${TC_BUILD:-container}" = host ] || [ -n "${TC_IN_BUILDER:-}" ]; then
    # Inside the builder (or on a host that already has the toolchain).
    if [ -n "${TC_IN_BUILDER:-}" ]; then
        apt-get update -qq
        apt-get install -y -qq musl-tools
        rustup target add "$TARGET"
    fi
    cd "$WS"
    exec cargo build --release --locked --target "$TARGET"
fi

# Root inside the container, because apt needs it. Under rootless podman the container's uid 0 maps
# to the invoking user, so target/ still comes back owned by the caller. :Z relabels for SELinux.
exec podman run --rm \
    -e TC_IN_BUILDER=1 \
    -e CARGO_HOME=/work/target/.cargo-home \
    -v "$WS:/work:Z" \
    -w /work \
    --entrypoint /work/deploy/build-musl.sh \
    "$BUILDER_IMAGE"
