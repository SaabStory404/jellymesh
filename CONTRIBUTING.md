# Contributing to JellyMesh

This page covers how to build, test, propose changes, and write documentation in this repository. It is for contributors to the code and the docs.

**Status:** Implemented. Commit and PR conventions are inferred from `git log` and are pending confirmation by the maintainer.

## Ways to help

| Way to help | Where to start |
| --- | --- |
| Fix or triage a bug | The repository issue tracker. Existing patches reference issues such as gh-14 and gh-15 in [docs/engineering/bughunt.md](docs/engineering/bughunt.md). |
| Build a planned feature | The Planned items in [docs/ROADMAP.md](docs/ROADMAP.md), for example playback observability, sticky server failover, the plugin compatibility layer, and the transcode pool items. |
| Improve documentation | The [doc TODO list](#doc-todo-list) below. |
| Report a measurement | Re-run a benchmark on different hardware and record the method and test bed (see [Evidence rules](#evidence-rules-for-claims)). |
| Report a security problem | Do not open a public issue. Follow [SECURITY.md](SECURITY.md), which is a draft: its reporting contact is not confirmed yet. |

For large changes, opening an issue first is a proposed practice, pending confirmation by the maintainer.

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

Build the patched Pomelo first, then run the tests. Use `-c Release`; the Debug analyzer set does not match (From code reading).

```bash
galera/pomelo/build.sh
dotnet test -c Release galera/Jellyfin.Database.Providers.Galera.Tests
```

Tests with `_Lab_` in the name run only when `JELLYMESH_TEST_DB` holds a MySQL or Galera connection string; without it they return immediately and pass (`galera/README.md`).

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

Build static binaries and run the protocol suite. It needs `ffmpeg`, and `musl-tools` or `CC_x86_64_unknown_linux_musl=gcc` because `ring` compiles C.

```bash
cargo build --release --locked --target x86_64-unknown-linux-musl
B=target/x86_64-unknown-linux-musl/release; AGENT=$B/tcpool-agent SHIM=$B/tcpool-shim bash spike/proto_test.sh
```

The protocol suite has 21 cases (`.github/workflows/transcode.yml`, `transcode/README.md`). CI also runs the `fuzz-smoke` job in the same workflow: nightly Rust, cargo-fuzz 0.13.2, 60 seconds per target (`validate`, `render`, `filters`, `trickplay`) against the committed corpus. A local fuzz command is Not documented yet.

### Images and CI

Two workflows exist. `transcode.yml` runs the checks and the protocol suite. `transcode-images.yml` publishes `ghcr.io/saabstory404/tcpool-agent:<sha>` and `tcpool-shim:<sha>` on pushes to `main` that touch `transcode/**`, and on manual dispatch; the shim image holds only the static shim and sync binaries. No workflow builds the `jellymesh-jellyfin` image, and none covers `galera/`, `leader/`, or `jellyfin-perf/`.

The `jellymesh-jellyfin` image is built manually with podman from a staging context that is not in the repo (see the doc TODO list):

```bash
podman build -f image/Containerfile.jm8.4 -t ghcr.io/saabstory404/jellymesh-jellyfin:12.1-jm8.4 <ctx>
```

`leader/` has no tests and no documented build command; its project file is `leader/JellyMesh.Leader.csproj`.

## Patch series rules

The series in `jellyfin-perf/bughunt/` applies to Jellyfin v12.1 after `jellyfin-12.1-perf.patch`. `build.sh` applies the files in numeric order. These rules are working practice inferred from the series and `docs/engineering/bughunt.md`; the maintainer has not confirmed them.

1. Name a new patch `NN-short-name.patch`, where `NN` is the next unused number in the directory.
2. Make the patch apply cleanly on top of every lower-numbered patch.
3. Put a behavior change that risks stock behavior behind an environment flag, with the flag off leaving behavior unchanged. Some patches are unconditional fixes (the summary table lists the flag as none). Flags in the series: `JELLYFIN_SHARED_DB=1` (patches 03, 07, 13, 15, 16 reference it), `JELLYFIN_SHARED_INVALIDATION` and `JELLYFIN_SHARED_INVALIDATION_POLL_MS` (patch 15), `JELLYMESH_SHARED_TRANSCODE_DIR=1` (patches 03 and 16), `JELLYMESH_DOVI_P7_TO_81=1` (patches 13, 17, 18, 19). `JELLYMESH_KEEPALIVE` is set by Jellyfin for patch 16, not by the operator.
4. Note any interface change. Patch 08 adds non-default members to `IMediaStreamRepository`, `IMediaAttachmentRepository`, `IMediaSegmentManager`, and `IMediaSourceManager`, which breaks plugins that implement them. Patch 07 adds a defaulted member to `IUserDataManager`.
5. Update the patch summary table in [docs/engineering/bughunt.md](docs/engineering/bughunt.md). The table currently has no rows for patches 14 and 15; add them when you touch that table.
6. Update the image tag mapping. The header comment of each `image/Containerfile.jmN` describes the delta from the previous tag; `image/Containerfile.jm8.4` states "series 00-19".
7. Build with `BUGHUNT=1 ./jellyfin-perf/build.sh` and run the Jellyfin test projects your patch touches.

## Commit and PR conventions

Inferred from `git log`; the maintainer has not confirmed them.

- Branch from `main` and open a pull request against it. Most history shows GitHub merge commits ("Merge pull request #N from ..."); some long-lived branches were merged directly.
- Name branches by type or topic, for example `fix/gh-15`.
- Write subjects that mostly start lowercase, with an optional scope prefix, for example `fix(fuzz): ...`, `docs(bughunt): ...`, `chore: ...`, `bughunt 19: ...`, `gh-14: ...`. Some subjects start with a capital (for example `Detached jobs: ...`).
- Run the Rust checks above before pushing changes under `transcode/`; CI runs them on pull requests that touch `transcode/**`.

## Evidence rules for claims

Every number in code comments, documentation, or a pull request description needs a source and a test bed.

- Label a result "Measured" only if you ran a command and can name it. Give the method in one sentence.
- Label a statement "From code reading" if you inferred it from source without running it. Label untested behavior "Not measured".
- State the test-bed context next to the number: hardware, single host or multiple hosts, sample size (for example n=1), and warm or cold cache. [docs/RESULTS.md](docs/RESULTS.md) gives the test bed of the existing results.
- When two sources disagree, state both with their method, or use the newer source and name it.
- Avoid absolute claims. Use the measured behavior instead: direct play needs a client Range retry after a replica hard kill, and an HLS transcode failover restarts ffmpeg.

## Documentation rules

[docs/README.md](docs/README.md) sorts pages by reader need (Diataxis: tutorial, how-to, reference, explanation). Each page under `docs/`, except `docs/engineering/`, has a Status line and ends with a Related docs list.

| Type | Pages |
| --- | --- |
| Tutorial | `docs/getting-started.md` |
| How-to | `docs/operations.md`, `docs/troubleshooting.md` |
| Reference | `docs/configuration.md`, component READMEs, `docs/RESULTS.md` |
| Explanation | `docs/architecture.md`, `docs/dolby-vision.md` |

Style rules (proposed by the maintainer for the doc set; only the structure rules above are stated in `docs/README.md`):

- **Voice:** direct, technical, present tense; "you" in instructions, third person for the system. American English. Write "the maintainer", not a personal name.
- **No hype:** state the measurement instead of adjectives. Describe stock Jellyfin behavior neutrally and do not disparage other projects.
- **Structure:** one H1 per file, sentence-case headings, a two-sentence summary and a Status line at the top, paragraphs of at most four sentences, and a Related docs list at the end.
- **Status labels:** Implemented, Production, Lab-verified, or Planned. Planned items link to `docs/ROADMAP.md` or an issue. Production means deployed on the maintainer's cluster as reported by the maintainer, with a date.
- **Component READMEs** use this order: title and one-sentence purpose; Status; What it does; Requirements; Build; Configure; Run or use; Test; Limitations; Related docs; License.
- **Code blocks** carry a language tag. Copy commands, environment variables, flags, and paths from code; do not invent them.
- **Links:** relative only. Link the first mention of a term to the [glossary](docs/architecture.md#glossary).
- **Banned words:** the doc set avoids the fixed list of words checked by the grep below. Run it on a page before you submit it.

  ```bash
  grep -n -i -E "hones|frank|candid|transparen|genuine|sincere|absolute|trul|to be cl" docs/*.md
  ```

- **Private details:** keep session narrative, internal identifiers, private paths, and private hostnames out of published docs. Working-log material lives in [docs/engineering/](docs/engineering/).

If a fact is missing, write "Not documented yet" and add an entry to the list below rather than guessing.

## Doc TODO list

Open gaps found while auditing the docs, checked against the tree on 2026-09-30.

- [ ] `transcode/deploy/CONTRACT.md`: says the agent does not read `TC_METRICS_PORT` (the agent reads it in `transcode/crates/agent/src/config.rs`), says `tcpool-sync` reads only `TC_WORKERS` (code implements `TC_WORKERS_DNS`), names the workflow `tcpool-images.yml` (the file is `transcode-images.yml`), and says the Dolby Vision wiring is "not in an image yet".
- [ ] `transcode/deploy/k8s/README.md` and `transcode/deploy/k8s/jellyfin-patch.md` are referenced from `CONTRACT.md` and several manifests, but neither file exists.
- [ ] `image/Containerfile` refers to `stage_jm3.py`, which is not in the repo. No README explains how the `Containerfile.jm5` to `Containerfile.jm8.4` layers chain or where the staging context comes from.
- [ ] `docs/RESULTS.md` cites `spikes/jellymesh/load.py` and `mesh/mesh_drill.py`, and `galera/tools/stmt_counts.py` and the latency table in `docs/engineering/galera-provider.md` cite `spikes/jellymesh`; none of those exist in this repo (`stmt_counts.py` still runs).
- [ ] No Traefik or Kubernetes manifests exist for the HA failover route or the Lease RBAC that `leader/` needs (get, create, update, patch on `leases` in the pod namespace); `transcode/deploy/k8s/50-rbac.yaml` holds only ServiceAccounts.
- [ ] The repo does not document how to prepare the Jellyfin config directory that `galera/lab/jf-galera.sh` reads from `JG_SRC` (scheduled tasks emptied, lab API key present), so `docs/getting-started.md` Steps 5 to 7 cannot be completed from the repo alone.
- [ ] The `dotnet build` commands and `bin/Release/net10.0` output paths for the provider and `galera/Jellyfin.DbMigrate` are derived from the csproj files and have not been run and recorded; confirm them and that the tool builds an executable named `jellyfin-dbmigrate`.
- [ ] `SECURITY.md`: the reporting channel, supported-versions policy, response and disclosure window, advisory process, and out-of-scope attack classes are Not documented yet.
- [ ] Galera provider: production server settings (grants, sizing, backup) and whether the provider runs on the maintainer's cluster are Not documented yet (`galera/README.md`).
- [ ] `galera/Jellyfin.DbMigrate/Program.cs`: `--probe` tests `Arg("--probe") is not null`, which returns the token after `--probe`; as the last argument it is null and a full `copy` runs. Fix the code (test for the flag itself); the option is undocumented until then.
- [ ] `leader/README.md` exists, but `leader/` has no documented build steps and no tests.
- [ ] `docs/engineering/bughunt.md` line 37 states "nothing had been built into an image or deployed" as of 2026-09-27, which predates the 2026-09-29 production report for Dolby Vision 7 -> 8.1.
- [ ] `docs/dolby-vision.md`: the `ffprobe` invocation for checking a converted `init.mp4` is not documented; DTS and DTS-HD in fMP4, clients other than the Android TV app, and AVR Dolby Digital Plus passthrough are not measured.
- [ ] `docs/configuration.md` and `docs/troubleshooting.md` link to `docs/BUGHUNT.md`; the file is `docs/engineering/bughunt.md`.
- [ ] `docs/troubleshooting.md`: no recorded fix for the node-local forgot-password PIN file (c1) or the per-node `MaxActiveSessions` cap (c2); no documented agent log location; no documented production `pxc_strict_mode` setting; whether CI runs the allowlist suite (134 accepted, 11 rejected) is not documented.
- [ ] `docs/RESULTS.md` gaps: the in-progress item count behind the Resume figures (0.14 s, 9.8 req/s, 33.4 ms p50); the CPU model and storage of the test host; the date of the concurrent-load runs; the run count and date of the authentication drill; why the 2-Jellyfin 32-client cell (120.2 req/s) differs from the `JELLYFIN_SHARED_DB` row (112.0); and reproduction commands for the transcode calibration, startup, fuzzing, and Dolby Vision census and proof results.
- [ ] `transcode/README.md` says of the protocol suite "The earlier README said 14; the workflow file is the newer source". Replace it with the plain count of 21 numbered cases.

## Code of conduct

This repository has no code of conduct. Keep discussion technical and respectful.

## Licensing

The repository license is GPL-2.0 (see [LICENSE](LICENSE)). `transcode/Cargo.toml` declares `license = "MIT"` and `leader/` declares none; [README.md](README.md) flags both for maintainer review, so the license of those directories is unsettled. The Pomelo patch in `galera/pomelo/` applies to Pomelo.EntityFrameworkCore.MySql. Whether contributions are accepted under the repository license is not stated in the repo and is pending the maintainer.

## Reporting security issues

Do not file public issues for vulnerabilities. Follow [SECURITY.md](SECURITY.md).

## Related docs

- [README.md](README.md)
- [docs/README.md](docs/README.md)
- [docs/architecture.md](docs/architecture.md)
- [docs/ROADMAP.md](docs/ROADMAP.md)
- [docs/RESULTS.md](docs/RESULTS.md)
- [docs/engineering/bughunt.md](docs/engineering/bughunt.md)
- [SECURITY.md](SECURITY.md)
