# JellyMesh Leader plugin

The leader plugin makes Jellyfin scheduled tasks and the library watcher run on one replica at a time. It elects that replica with a Kubernetes Lease (an object with a holder and an expiry; see the [glossary](../docs/architecture.md#glossary)).

**Status:** Implemented. Election and task forwarding have no tests and no measured failover time; the behavior below is From code reading.

## What it does

Each replica competes for one Lease. The holder is the leader. The leader starts `ILibraryMonitor`; followers stop it, and the plugin undoes Jellyfin's own start of the monitor once a follower is more than 30 s past startup.

A scheduled task that starts on a follower is cancelled there and forwarded to the leader through the Lease annotation `jellymesh.io/run-task`. The leader runs it through `ITaskManager.Execute` if the worker is idle.

Outside Kubernetes the plugin does no election: it sets `IsLeader` to true and the node runs its own tasks. The plugin decides it is inside Kubernetes when `KUBERNETES_SERVICE_HOST` is set and the service account token file exists.

## Requirements

- Jellyfin 12.1. The plugin targets net10.0 and references `Jellyfin.Controller` and `Jellyfin.Model` 12.1.0 (`JellyMesh.Leader.csproj`).
- On Kubernetes, the pod's service account needs `get`, `create`, `update`, and `patch` on `leases` in the pod's own namespace.
- Replica hostnames must differ, because the holder identity is `HOSTNAME`.

The repo has no RBAC manifest. This Role is an example derived from the comment in `LeaseLeaderService.cs`:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: jellyfin-leader
rules:
  - apiGroups: ["coordination.k8s.io"]
    resources: ["leases"]
    verbs: ["get", "create", "update", "patch"]
```

## Build

Not documented yet. The project file is `JellyMesh.Leader.csproj`; the build and packaging steps are on the doc TODO list in [CONTRIBUTING](../CONTRIBUTING.md#doc-todo-list).

## Configure

The plugin has no configuration page. Two environment variables set its behavior; the full table is in [configuration](../docs/configuration.md).

| Variable | Default | Source |
| --- | --- | --- |
| `JELLYMESH_LEASE` | `jellyfin-tasks` | `LeaseLeaderService.cs` |
| `JELLYMESH_LEASE_SECONDS` | `15` | `LeaseLeaderService.cs` |

The plugin creates the Lease if it does not exist. A concurrent takeover fails with HTTP 409 because the update carries `resourceVersion`. The plugin talks to the Kubernetes API over HTTPS with the service account token and cluster CA, with a 5 s timeout.

## Run or use

Install the plugin folder on every replica. In the JellyMesh image, `image/install-plugins.sh` does this from an initContainer (see [image](../image/README.md)). The log line `JellyMesh Leader: <identity> is now LEADER` or `follower` shows the role.

The election loop ticks every max(1, `JELLYMESH_LEASE_SECONDS` / 5) seconds, which is 3 s at the default. If the leader stops without a graceful shutdown, a follower takes over after the lease expires plus up to one tick: about 18 s at the default (From code reading, not measured). On graceful stop the leader clears `holderIdentity` and handover is immediate.

## Test

The directory has no tests. Adding them is on the doc TODO list in [CONTRIBUTING](../CONTRIBUTING.md#doc-todo-list).

## Limitations

- Task forwarding is best-effort. There is one annotation slot, so two forwarded tasks close together can overwrite each other.
- A forwarded task is skipped, with a log line, if the leader's worker is not idle.
- The follower cancels its own run of the task. What a client progress display shows for that run is not documented.
- The leader steps down on any API error or failed renew.
- A named database lock (`GET_LOCK`) cannot elect a leader, because Galera does not replicate named locks.

## Related docs

- [Operations](../docs/operations.md): where the leader plugin fits in a deployment.
- [Configuration reference](../docs/configuration.md): environment variables.
- [Architecture](../docs/architecture.md): scheduled tasks and the glossary.

## License

GPL-2.0 for the repository as a whole ([LICENSE](../LICENSE)). The project file declares no separate license.
