# Security policy

This file explains how to report a vulnerability in JellyMesh and lists the security-relevant behavior of each component as it exists in the repository. Statements in the notes section cite their source file.

**Status:** Implemented. Report vulnerabilities privately through GitHub private vulnerability reporting.

## Supported versions

The project publishes no supported-versions policy. Fixes are made on `main`; the `jellymesh-jellyfin` image is built manually with podman from it (see [Contributing](CONTRIBUTING.md)).

## Reporting a vulnerability

Do not open a public issue for a vulnerability.

Report it through GitHub private vulnerability reporting: open the repository's **Security** tab and choose **Report a vulnerability**.

A useful report includes:

- The component and the image tag or commit.
- The configuration involved (environment variables from [configuration](docs/configuration.md)).
- Steps to reproduce.

## Response and disclosure

The project publishes no response-time or disclosure-window commitment.

## Scope

In scope are the components in this repository.

| Component | Path | In scope |
|---|---|---|
| Galera database provider and `jellyfin-dbmigrate` | `galera/` | Yes |
| Pomelo patch | `galera/pomelo/` | Yes, the patch only |
| Jellyfin patch series and bughunt patches | `jellyfin-perf/` | Yes, the patches only |
| Leader plugin | `leader/` | Yes |
| Transcode pool crates: `tcpool-agent`, `tcpool-shim`, `tcpool-sync`, `tcpool-ir`, `tcpool-proto` | `transcode/crates/` | Yes |
| Transcode deployment: manifests, Containerfiles, alert rules | `transcode/deploy/` | Yes |
| Container image layers and install script | `image/` | Yes |
| Lab scripts and lab tooling | `galera/lab/`, `galera/tools/`, `tools/` | No; hardening is out of scope, see "Lab-only material" |

Report problems in upstream Jellyfin, Pomelo, or ffmpeg to those projects. If a JellyMesh patch introduces or exposes the problem, report it here.

Beyond the lab-only material below, no attack class is excluded from scope.

## Security model and known observations

The notes below cite repository files. "From code reading" means the statement comes from reading source and was not tested. Terms such as mTLS, Lease, Galera, and Pomelo are defined in the [glossary](docs/architecture.md#glossary).

### Transcode pool

Status: Implemented. The agent, shim, and sync are built from `transcode/crates/`; the deployment contract is `transcode/deploy/CONTRACT.md`.

| Topic | Behavior | Source |
|---|---|---|
| Transport and client identity | gRPC on port 9901 over mTLS. Agents require a client certificate signed by the pool CA, and clients verify the agent against the same CA. The pool uses its own cert-manager CA, not the cluster CA. That CA signs nothing else, so any certificate it issued is a pool identity. `tcpool-client-tls` is limited to `client auth` and is used by the shim and `tcpool-sync`. | `transcode/crates/proto/src/tls.rs`; [CONTRACT.md](transcode/deploy/CONTRACT.md) |
| Server name | `TC_TLS_SERVER_NAME` defaults to `tcpool-agent`, a SAN on every agent certificate. The agent SANs are `tcpool-agent`, `tcpool-agents`, and `tcpool-agents.media.svc.cluster.local`. The shim overrides the domain name with it, which lets it dial bare pod IPs. | `tls.rs`; CONTRACT.md |
| Health port | When TLS is on, the agent also serves gRPC health in plaintext on `TC_HEALTH_PORT` (default 9902; the CONTRACT sets 9902), because the kubelet gRPC probe cannot speak TLS. Port 9902 exposes health and nothing else. | `transcode/crates/agent/src/main.rs`; CONTRACT.md |
| Metrics ports | The agent serves Prometheus `/metrics` on 9903 and `tcpool-sync` serves `/metrics` and `/status` on 9904, each over plain HTTP and only when `TC_METRICS_PORT` is set. The manifests set 9903 on the agents. | `transcode/crates/agent/src/metrics.rs`; `transcode/crates/sync/src/metrics.rs`; CONTRACT.md |
| Missing or partial certificates, agent | A partial set of `TC_TLS_CERT`, `TC_TLS_KEY`, `TC_TLS_CA` is always an error. No TLS variables is an error when `TC_TLS_REQUIRED` is set to anything other than `0`. Without TLS variables and without `TC_TLS_REQUIRED`, the agent serves plaintext and logs "WARNING plaintext gRPC (no TC_TLS_*): lab/dev only". The agent calls `exit(2)` on the TLS error paths in `main.rs`. The CONTRACT sets `TC_TLS_REQUIRED=1`. | `tls.rs`; `agent/src/main.rs` |
| Missing or partial certificates, shim | On TLS misconfiguration the shim logs "tls misconfigured ... running LOCALLY" and runs the transcode on the local CPU. It does not fall back to plaintext and does not fail the session. | `transcode/crates/shim/src/main.rs` |
| Certificate files | `/tls` is mounted read-only with mode 0440 and needs `fsGroup: 1000` on the pod. Without it the agent (uid 1000) gets EACCES on its private key, exits 2 and CrashLoops, and the shim silently CPU-encodes every session. | CONTRACT.md |
| Certificate lifetime and keys | Leaf certificates last 90 days and renew at 30 days. Keys are RSA 2048 in PKCS8 with `rotationPolicy: Always`. | CONTRACT.md |
| Certificate rotation | The agent watches the certificate files and drains itself when they change. cert-manager does not reissue leaf certificates when the CA rotates, so the pool runs on mixed trust for up to 60 days unless you delete `tcpool-agent-tls` and `tcpool-client-tls` in the same change as the CA rotation. | CONTRACT.md |
| Command allowlist | The agent validates each ffmpeg command line before running it, as defense in depth behind mTLS. The validator accepts only Jellyfin's grammar: no `concat:`, `http:`, or other protocols, and unknown shapes run on the CPU chain or are rejected. Inputs must be under `TC_INPUT_ROOTS` (default `/media,/data/media`), reads under `TC_READ_ROOTS`, and HLS outputs under `TC_OUTPUT_ROOT`. Trickplay jobs write only under `TC_TRICKPLAY_OUTPUT_ROOT`; when it is unset, every trickplay job is refused. | `transcode/crates/ir/src/validate.rs`; `transcode/crates/agent/src/config.rs`; [transcode-plan.md](docs/engineering/transcode-plan.md) |
| Allowlist test result | The plan records a checked item "Command allowlist (134 real commands pass, 11 attacks rejected) - case 13". This is a plan checkbox. The test source for case 13 was not re-run or re-read for this file, and the result is not an audit. The plan also lists fuzzing (`cargo-fuzz` targets including `validate`). | transcode-plan.md |
| Mounts | `/data/media` is mounted read-only. The two Jellyfin data hostPaths use `type: Directory`, not `DirectoryOrCreate`, so a missing directory fails the pod instead of the kubelet creating a root-owned directory in the Jellyfin config tree. | CONTRACT.md |
| Images | The agent runs as uid 1000. The CONTRACT states that images are pinned by digest and tagged by git SHA, never `:latest`, and that the base image must match Jellyfin's. That statement was not re-checked against the manifests here. | CONTRACT.md |
| Kubernetes API access | The pool ServiceAccounts set `automountServiceAccountToken: false`, so pool pods get no API token. The file defines no Role or RoleBinding, and its comment says neither the agent nor sync talks to the Kubernetes API. | `transcode/deploy/k8s/50-rbac.yaml` |
| Sync API key | `tcpool-sync` reads a Jellyfin API key from the `tcpool-sync` Secret (key `api-key`). The Secret is supplied out of band and must not be in this repo. | CONTRACT.md |

Port and variable reference: [configuration](docs/configuration.md).

### Database provider

Status: Implemented, Lab-verified.

| Topic | Behavior | Source |
|---|---|---|
| Password source | `JELLYMESH_DB_PASSWORD`, when non-empty, overrides the password in the connection string. This keeps the password out of `database.xml`, which appears in configuration backups. The intended source is a Kubernetes Secret. | `galera/Jellyfin.Database.Providers.Galera/GaleraDatabaseProvider.cs` |
| Logging | The connection string is logged with the password masked as `*****`. | `GaleraDatabaseProvider.cs` |
| Backups | The provider does not back up the database. `MigrationBackupFast` only sets the database character set and collation and logs a warning. `DeleteBackup` does nothing, and `RestoreBackupFast` logs a critical message and does nothing. Back up the cluster yourself. | `GaleraDatabaseProvider.cs` |
| `jellyfin-dbmigrate copy` | Deletes all existing rows in every non-empty table of the target before it writes the source rows. Jellyfin must be stopped, and `--to` must point at an empty or disposable database. | `galera/Jellyfin.DbMigrate/Program.cs` |

### Leader plugin

Status: Implemented.

| Topic | Behavior | Source |
|---|---|---|
| RBAC | The pod ServiceAccount needs `get`, `create`, `update`, and `patch` on `leases` in its own namespace. The plugin talks to the Kubernetes API over HTTPS with the ServiceAccount token and cluster CA. Grant only those verbs in that namespace. The repo has no RBAC manifest for the leader; [operations](docs/operations.md) gives an example Role. | `leader/LeaseLeaderService.cs`; docs/operations.md |
| Lease handling | The plugin creates the Lease if it does not exist. A concurrent takeover fails with HTTP 409 because the update carries `resourceVersion`. | docs/operations.md |
| Outside Kubernetes | Without `KUBERNETES_SERVICE_HOST` and a ServiceAccount token, no Lease is used and the node runs scheduled tasks itself. | `leader/LeaseLeaderService.cs` |

### Stock Jellyfin behavior observed in the lab

Status: Lab-verified for the first and third rows; the second row is a deployment observation.

| Topic | Behavior | Source |
|---|---|---|
| Unauthenticated range requests | In the lab, `/Videos/{id}/stream` and `/Audio/{id}/stream` answered range requests without a token; the doc marks this as measured on stock Jellyfin. The cause, from code reading, is that the controller actions carry no `[Authorize]` attribute and the API has no global authorization filter. An authenticated Range retry was not measured. This belongs upstream. The example Traefik route in `deploy/examples/` does not restrict these paths; any restriction is up to the operator. | [direct-play-failover.md](docs/engineering/direct-play-failover.md) |
| `api_key` in Traefik logs | Traefik access logs recorded the `api_key` query parameter in cleartext for two lab services. This is not fixed in the repo. Restrict access to those logs, or move the services to header authentication. | [bughunt.md](docs/engineering/bughunt.md); [troubleshooting](docs/troubleshooting.md) |
| Session cap per node (`c2`) | `MaxActiveSessions` is checked against a per-process in-memory dictionary, so a user can exceed the cap by splitting sessions across HA replicas. From code reading only; not fixed in the repo. | bughunt.md |

### Lab-only material

Status: Implemented as lab tooling; not for production.

| Topic | Behavior | Source |
|---|---|---|
| Database credentials | The lab connection string uses user `jellyfin` with password `jellyfin`, `SslMode=Disabled`, and `AllowPublicKeyRetrieval=true`. With `JG_PW_ENV=1` the script also passes `JELLYMESH_DB_PASSWORD=jellyfin`. | `galera/lab/jf-galera.sh` |
| Root password | The default Galera root password is `labroot` (override with `GL_ROOTPW`). | `galera/lab/galera-lab.sh` |
| API key | `JG_SRC` names a prepared config directory that holds a lab API key. That directory is outside the repo, and no script embeds a key. | `galera/lab/jf-galera.sh` |

Do not use any of these values or settings in production.

## Related docs

- [Architecture](docs/architecture.md)
- [Configuration reference](docs/configuration.md)
- [Operations](docs/operations.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Transcode deployment contract](transcode/deploy/CONTRACT.md)
- [Contributing](CONTRIBUTING.md)
