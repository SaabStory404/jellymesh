# Contributing to JellyMesh

How to build JellyMesh, test it, get a change in, and write the docs.

## Ways to help

| Way to help | Where to start |
| --- | --- |
| Fix or triage a bug | The repository issue tracker. Existing patches reference issues such as gh-14 and gh-15 in [docs/engineering/bughunt.md](docs/engineering/bughunt.md). |
| Build a planned feature | The Planned items in [docs/ROADMAP.md](docs/ROADMAP.md), for example playback observability, sticky server failover, the plugin compatibility layer, and the transcode pool items. |
| Improve documentation | The [open documentation gaps](#open-documentation-gaps) below. |
| Report a measurement | Re-run a benchmark on different hardware and record the method and test bed (see [Evidence rules](#evidence-rules-for-claims)). |
| Report a security problem | Do not open a public issue. Use GitHub private vulnerability reporting as described in [SECURITY.md](SECURITY.md). |

For a large change, open an issue first so we agree on the approach before you write the code.

## Repo map

| Directory | Language | Purpose |
| --- | --- | --- |
| `galera/` | C# (.NET 10) | Galera provider plugin and the Pomelo patch |
| `jellyfin-perf/` | Patch series | Patches applied to Jellyfin v12.1 |
| `leader/` | C# (`net10.0` Jellyfin plugin) | Lease-based leader election for scheduled tasks |
| `transcode/` | Rust workspace | GPU transcode pool |
| `image/` | Containerfiles (podman) | `jellymesh-jellyfin` images |
| `tools/` | Shell, JSON, text | Measurement scripts; the Dolby Vision FEL residual write-up is [docs/engineering/gh15-fel-visibility.md](docs/engineering/gh15-fel-visibility.md) |
| `docs/` | Markdown | See [Documentation rules](#documentation-rules) |

### Building and testing galera/

Build the patched Pomelo first, then run the tests. Use `-c Release`: as far as I can tell from the project files, the Debug analyzer set doesn't match.

```bash
galera/pomelo/build.sh
dotnet test -c Release galera/Jellyfin.Database.Providers.Galera.Tests
```

Tests with `_Lab_` in the name only run when `JELLYMESH_TEST_DB` holds a MySQL or Galera connection string. Without it they return immediately and pass, which is easy to mistake for a real green run.

### Building jellyfin-perf/

`build.sh` needs a dotnet SDK. It clones Jellyfin into `JF_SRC` (default `~/.cache/jellymesh-vendor/jellyfin-src`) and writes the changed assemblies to `JF_OVERLAY` (default `~/.cache/jellymesh-vendor/jellyfin-perf`).

```bash
BUGHUNT=1 ./jellyfin-perf/build.sh
```

`BUGHUNT=1` applies the perf patch plus `bughunt/NN-*.patch`. `BUGHUNT=0` applies the perf patch alone.

### Building and testing the Rust workspace

Run these from `transcode/` (`rust-version` is 1.88 in `transcode/Cargo.toml`). They match the checks in `.github/workflows/transcode.yml`.

```bash
cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
```

Build static binaries and run the protocol suite. It needs `ffmpeg`, and `musl-tools` or `CC_x86_64_unknown_linux_musl=gcc`, because `ring` compiles C.

```bash
cargo build --release --locked --target x86_64-unknown-linux-musl
B=target/x86_64-unknown-linux-musl/release; AGENT=$B/tcpool-agent SHIM=$B/tcpool-shim bash spike/proto_test.sh
```

The protocol suite has 21 cases. CI also runs the `fuzz-smoke` job in the same workflow: nightly Rust, cargo-fuzz 0.13.2, 60 seconds per target (`validate`, `render`, `filters`, `trickplay`) against the committed corpus. To run the same fuzz smoke locally, install the tools once and run each target from `transcode/`:

```bash
rustup toolchain install nightly --profile minimal
rustup +nightly component add rust-src
cargo install cargo-fuzz --version 0.13.2 --locked
cargo +nightly fuzz build
for t in validate render filters trickplay; do
  cargo +nightly fuzz run "$t" "fuzz/corpus/$t" -- -max_total_time=60 -timeout=10
done
```

A crash, timeout, or out-of-memory makes `cargo fuzz run` exit non-zero and leaves the reproducer under `transcode/fuzz/artifacts/`.

### Images and CI

There are two workflows. `transcode.yml` runs the checks and the protocol suite. `transcode-images.yml` publishes `ghcr.io/saabstory404/tcpool-agent:<sha>` and `tcpool-shim:<sha>` on pushes to `main` that touch `transcode/**`, and on manual dispatch; the shim image holds only the static shim and sync binaries. Nothing in CI builds the `jellymesh-jellyfin` image, and nothing covers `galera/`, `leader/`, or `jellyfin-perf/`.

I build the `jellymesh-jellyfin` image by hand with podman. The staging context (`<ctx>` below) gets assembled per deployment and isn't shipped here; [image/README.md](image/README.md) lists what each Containerfile expects to find in it.

```bash
podman build -f image/Containerfile.jm8.4 -t ghcr.io/saabstory404/jellymesh-jellyfin:12.1-jm8.4 <ctx>
```

### Building leader/

Build `leader/` with the .NET 10 SDK. That builds clean; the output is `leader/bin/Release/net10.0/JellyMesh.Leader.dll`. The directory has no tests.

```bash
dotnet build -c Release leader/JellyMesh.Leader.csproj
```

## Patch series rules

The series in `jellyfin-perf/bughunt/` applies to Jellyfin v12.1 after `jellyfin-12.1-perf.patch`, and `build.sh` applies the files in numeric order. Follow these rules for every new patch.

1. Name a new patch `NN-short-name.patch`, where `NN` is the next unused number in the directory.
2. Make the patch apply cleanly on top of every lower-numbered patch.
3. Put a behavior change that risks stock behavior behind an environment flag, with the flag off leaving behavior unchanged. Some patches are unconditional fixes, and the summary table lists the flag as none. Flags in the series: `JELLYFIN_SHARED_DB=1` (patches 03, 07, 13, 15, 16 reference it), `JELLYFIN_SHARED_INVALIDATION` and `JELLYFIN_SHARED_INVALIDATION_POLL_MS` (patch 15), `JELLYMESH_SHARED_TRANSCODE_DIR=1` (patches 03 and 16), `JELLYMESH_DOVI_P7_TO_81=1` (patches 13, 17, 18, 19). `JELLYMESH_KEEPALIVE` is set by Jellyfin for patch 16, not by the operator.
4. Note any interface change. Patch 08 adds non-default members to `IMediaStreamRepository`, `IMediaAttachmentRepository`, `IMediaSegmentManager`, and `IMediaSourceManager`, which breaks plugins that implement them. Patch 07 adds a defaulted member to `IUserDataManager`.
5. Update the patch summary table in [docs/engineering/bughunt.md](docs/engineering/bughunt.md). It has no rows for patches 14 and 15 yet; add them when you touch that table.
6. Update the image tag mapping. The header comment of each `image/Containerfile.jmN` describes the delta from the previous tag; `image/Containerfile.jm8.4` states "series 00-19".
7. Build with `BUGHUNT=1 ./jellyfin-perf/build.sh` and run the Jellyfin test projects your patch touches.

## Commit and PR conventions

- Branch from `main` and open a pull request against it. Most history shows GitHub merge commits ("Merge pull request #N from ..."); some long-lived branches were merged directly.
- Name branches by type or topic, for example `fix/gh-15`.
- Write subjects that mostly start lowercase, with an optional scope prefix, for example `fix(fuzz): ...`, `docs(bughunt): ...`, `chore: ...`, `bughunt 19: ...`, `gh-14: ...`. Some subjects start with a capital, for example `Detached jobs: ...`.
- Run the Rust checks above before pushing changes under `transcode/`; CI runs them on pull requests that touch `transcode/**`.

## Evidence rules for claims

Every number you put in a code comment, a doc, or a pull request description needs to carry where it came from.

- Say it in the same sentence as the number: what hardware, one host or several, one run or many, warm or cold cache. [docs/RESULTS.md](docs/RESULTS.md) gives the test bed for the numbers already here, so follow its shape. Don't tag sentences with labels; write the qualification into the prose.
- If you worked something out by reading the source rather than running it, say so plainly in the sentence. If nobody has run it, say nobody has run it.
- When two sources disagree, give both with their methods, or use the newer one and say which it is.
- Don't state absolutes. Say what was measured: direct play needs a client Range retry after a replica hard kill, and an HLS transcode failover restarts ffmpeg.

## Documentation rules

[docs/README.md](docs/README.md) sorts pages by what the reader is trying to do.

| Type | Pages |
| --- | --- |
| Tutorial | `docs/getting-started.md` |
| How-to | `docs/operations.md`, `docs/troubleshooting.md` |
| Reference | `docs/configuration.md`, component READMEs, `docs/RESULTS.md` |
| Explanation | `docs/architecture.md`, `docs/dolby-vision.md` |

Style rules for the doc set:

- **Voice:** direct, technical, present tense. Write in first person for your own experience with the code, "you" for the reader, and "it" or the component's name for the system. American English.
- **No hype:** state the measurement instead of adjectives. Describe stock Jellyfin behavior neutrally and do not disparage other projects.
- **Structure:** one H1 per file, sentence-case headings, paragraphs of at most four sentences. Lead with what the thing is and why you'd want it. Say up front, in one plain sentence, if a thing is opt-in, off by default, untested, or only run in a lab; every other caveat goes where the reader hits it. Don't hang a status banner off the top of the page.
- **Component READMEs** use this order: title and one-sentence purpose; What it does; Requirements; Build; Configure; Run or use; Test; Limitations; License.
- **Code blocks** carry a language tag. Copy commands, environment variables, flags, and paths from code; do not invent them.
- **Links:** relative only. Link an obscure term to the [glossary](docs/architecture.md#glossary) the first time it appears, if the page doesn't explain it inline.
- **Banned words:** the doc set avoids the fixed list of words checked by the grep below. Run it on a page before you submit it.

  ```bash
  grep -n -i -E "hones|frank|candid|transparen|genuine|sincere|absolute|trul|to be cl" docs/*.md
  ```

- **Private details:** keep session narrative, internal identifiers, private paths, and private hostnames out of published docs. Working-log material lives in [docs/engineering/](docs/engineering/).

If a fact is missing, say so in one plain sentence and add an entry to the list below rather than guessing.

## Open documentation gaps

Facts the repository can't supply, checked against the tree on 2026-09-30. The deployment-specific ones stay with the deployment; the rest need a run or a code change.

- [ ] Up to the deployment, so not in this repo: the Jellyfin-side pool configuration that `transcode/deploy/k8s/*.yaml` calls `k8s/README.md` and `k8s/jellyfin-patch.md`, the image staging context (including the `stage_jm3.py` named in `image/Containerfile`), the image tag production runs, the rollout state of the pool beyond Dolby Vision, and production MySQL or PXC settings (grants, sizing, backup).
- [ ] The lab's prepared Jellyfin config directory (`JG_SRC` in `galera/lab/jf-galera.sh`: scheduled tasks emptied, lab API key present) isn't scripted, so `docs/getting-started.md` Steps 5 to 7 need one you prepare yourself.
- [ ] The load, per-call, and coherence drivers (`spikes/jellymesh/load.py`, `spikes/jellymesh/bench.py`, `mesh/mesh_drill.py`) aren't in the repository, so those `docs/RESULTS.md` tables can't be regenerated from it. Nothing here reproduces the transcode calibration, startup latency, or Dolby Vision census and proof either.
- [ ] `docs/RESULTS.md` source gaps: the in-progress item count behind the Resume figures (0.14 s, 9.8 req/s, 33.4 ms p50); the CPU model and storage of the test host; the date of the concurrent-load runs; the run count and date of the authentication drill; and why the 2-Jellyfin 32-client cell (120.2 req/s) differs from the `JELLYFIN_SHARED_DB` row (112.0).
- [ ] `galera/Jellyfin.DbMigrate` has never been built from this tree: it needs `Jellyfin.Database.Providers.Sqlite.dll` from a Jellyfin 12.1 image. The Pomelo and provider builds were run on 2026-09-30.
- [ ] `galera/Jellyfin.DbMigrate/Program.cs`: `--probe` tests `Arg("--probe") is not null`, which returns the token after `--probe`; as the last argument it is null and a full `copy` runs. The code should test for the flag itself.
- [ ] Dolby Vision: DTS and DTS-HD audio in fMP4, and clients other than the Android TV app, have never been played through a converting job.
- [ ] Nothing ships a fix for the node-local forgot-password PIN file (`c1`) or the per-node `MaxActiveSessions` cap (`c2`); see [docs/troubleshooting.md](docs/troubleshooting.md).

## Code of conduct

This repository has no code of conduct. Keep discussion technical and respectful.

## Licensing

The repository license is GPL-2.0 (see [LICENSE](LICENSE)). `transcode/Cargo.toml` declares `license = "MIT"` and `leader/` declares none; I've flagged both in [README.md](README.md) and haven't settled them, so the license of those two directories is still open. The Pomelo patch in `galera/pomelo/` applies to Pomelo.EntityFrameworkCore.MySql. I haven't said anywhere whether contributions are accepted under the repository license, and I still need to decide that.

## Reporting security issues

Don't file public issues for vulnerabilities. Follow [SECURITY.md](SECURITY.md).
