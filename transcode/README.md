# transcode: a GPU transcode pool for Jellyfin

JellyMesh runs several Jellyfin servers. This directory makes their transcoding a shared pool
of mixed GPUs (currently an Intel Arc A380 over QSV and an NVIDIA Tesla P4 over NVENC, plus a CPU
spill worker). A viewer is never interrupted when a card, node or worker goes away.

Jellyfin itself is not modified for this. It runs with hardware acceleration **none**, so it
emits a portable software ffmpeg command line.
- `tcpool-shim` takes the place of Jellyfin's ffmpeg. HLS transcodes go to a worker; everything
  else runs the real ffmpeg locally.
- `tcpool-agent`, one per GPU, validates the command against an allowlist and rewrites it for its
  card (encoder, hardware decode, GPU scale/tonemap chain). It runs ffmpeg into a shared scratch
  directory, and turns jobs away when the card is full.
- `tcpool-sync` intersects what every worker can output and sets Jellyfin's HEVC/AV1 offers to
  that lowest common denominator.
- `tcpool-ir` is the shared translation and allowlist library.

If a worker dies before the first segment, the shim re-runs the job elsewhere. If it dies after,
Jellyfin's own HLS restart resumes at the next missing segment on another worker. A worker that
loses its shim fences its ffmpeg within 3 s, so two workers never write the same output.

| Path | What |
|---|---|
| `crates/` | Rust workspace: `ir`, `proto` (gRPC + mTLS), `agent`, `shim`, `sync` |
| `corpus/` | Real Jellyfin 12 command lines (media paths anonymized) + parity goldens |
| `fuzz/` | cargo-fuzz targets for the allowlist, renderer and filter parser |
| `calibration/` | VMAF / bitrate / latency measurements per card and settings |
| `deploy/` | Container images, entrypoint, alert rules, the Jellyfin/JellyMesh integration contract |
| `spike/` | The Python prototype the Rust port is pinned to (goldens), and the protocol suite `proto_test.sh` |
| `docs/PLAN.md` | Production plan, phases, live checklist, Jellyfin UI integration |

## Build and test

```sh
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test
# static binaries (ring needs a musl C compiler: musl-tools, or CC_x86_64_unknown_linux_musl=gcc)
cargo build --release --target x86_64-unknown-linux-musl
# protocol suite (14 cases: failover, fence, stall, pause, drain, allowlist, mTLS); needs ffmpeg
B=target/x86_64-unknown-linux-musl/release
AGENT=$B/tcpool-agent SHIM=$B/tcpool-shim bash spike/proto_test.sh
```

CI: `.github/workflows/transcode.yml`. Images: `.github/workflows/transcode-images.yml`.

Imported 2026-09-27 from the private repo where it was developed, as one sanitized commit:
media filenames were anonymized consistently (the goldens still match) and infrastructure names
were replaced.
