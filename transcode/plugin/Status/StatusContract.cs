using System.Text.Json;
using System.Text.Json.Serialization;

namespace JellyMesh.TranscodePool.Status;

// ---------------------------------------------------------------------------------------------
// THE WIRE FORMAT FILE. tcpool-sync's /status document is the only thing in the plugin that is
// not under our control, so it lives here and nowhere else: the DTOs, their JSON names and the
// parse/serialize pair. When sync's shape changes, this is the file to fix.
//
// Source of truth: crates/sync/src/status.rs (`render_json`), which is itself the PLAN §11 shape:
// schema, generated/synced timestamps, workers_configured/live, common_outputs, offers (with the
// intent-vs-effective cause), workers (state, capabilities, units), pool totals and the notes that
// explain what sync cannot see yet.
//
// Nothing here talks to the network and nothing here needs Jellyfin: it is pure
// string <-> object, so `StatusContractTests` can be added later without a host to run in. The
// names are pinned with [JsonPropertyName] rather than a naming policy so that Jellyfin's MVC
// serializer (PascalCase by default, JsonDefaults.PascalCaseOptions) emits exactly this shape too.
// ---------------------------------------------------------------------------------------------

/// <summary>
/// Sync's <c>/status</c> document. A snapshot of the pool as sync last reconciled it; it is not a
/// live view, so every timestamp in it matters.
/// </summary>
public sealed class PoolStatus
{
    /// <summary>
    /// The wire-format version sync emits. <see cref="StatusContract.SupportedSchema"/> is the one
    /// this build understands.
    /// </summary>
    [JsonPropertyName("schema")]
    public int Schema { get; set; }

    /// <summary>When this document was rendered (the server's clock).</summary>
    [JsonPropertyName("generated_unix")]
    public long GeneratedUnix { get; set; }

    /// <summary>When the sync cycle that produced it last started.</summary>
    [JsonPropertyName("synced_unix")]
    public long SyncedUnix { get; set; }

    /// <summary>When Jellyfin's encoding config was last changed by sync. 0 = never.</summary>
    [JsonPropertyName("last_success_unix")]
    public long LastSuccessUnix { get; set; }

    /// <summary>Workers sync knows about: discovered this cycle plus the ones persisted from earlier.</summary>
    [JsonPropertyName("workers_configured")]
    public int WorkersConfigured { get; set; }

    /// <summary>How many of those answered this cycle.</summary>
    [JsonPropertyName("workers_live")]
    public int WorkersLive { get; set; }

    /// <summary>Output tokens every configured worker can produce (the intersection).</summary>
    [JsonPropertyName("common_outputs")]
    public IReadOnlyList<string> CommonOutputs { get; set; } = [];

    /// <summary>The per-codec offers, in sync's fixed order.</summary>
    [JsonPropertyName("offers")]
    public IReadOnlyList<OfferStatus> Offers { get; set; } = [];

    /// <summary>Every configured worker, live or not.</summary>
    [JsonPropertyName("workers")]
    public IReadOnlyList<WorkerStatus> Workers { get; set; } = [];

    /// <summary>Pool-wide unit totals, counted over the live workers only.</summary>
    [JsonPropertyName("pool")]
    public PoolTotals Pool { get; set; } = new();

    /// <summary>
    /// Pool emergency mode, or <see langword="null"/> when unknown. Sync does not read the
    /// scheduling agent's metrics (:9903) yet, so today this is always null and
    /// <see cref="Notes"/> says why. Jellyfin's MVC serializer drops nulls, so the page must read
    /// an <em>absent</em> property as "unknown", not as "off".
    /// </summary>
    [JsonPropertyName("emergency")]
    public bool? Emergency { get; set; }

    /// <summary>What sync cannot report yet, and why, in sync's own words.</summary>
    [JsonPropertyName("notes")]
    public IReadOnlyList<string> Notes { get; set; } = [];
}

/// <summary>
/// One Jellyfin encoding-config offer after reconciliation. <c>effective = intent AND pool-can</c>,
/// and <see cref="Cause"/> is the decisive half — so the page can say "you turned it off" rather
/// than just showing a greyed checkbox.
/// </summary>
public sealed class OfferStatus
{
    /// <summary>Jellyfin's encoding-config key, e.g. <c>AllowHevcEncoding</c>.</summary>
    [JsonPropertyName("option")]
    public string Option { get; set; } = string.Empty;

    /// <summary>The output token the key gates, e.g. <c>hevc</c>.</summary>
    [JsonPropertyName("token")]
    public string Token { get; set; } = string.Empty;

    /// <summary>The operator's choice, persisted across cycles.</summary>
    [JsonPropertyName("intent")]
    public bool Intent { get; set; }

    /// <summary>What sync last left in Jellyfin's config: what Jellyfin actually offers.</summary>
    [JsonPropertyName("effective")]
    public bool Effective { get; set; }

    /// <summary>Why <see cref="Effective"/> is what it is: <c>enabled</c>, <c>user_disabled</c> or <c>pool_cannot</c>.</summary>
    [JsonPropertyName("cause")]
    public string Cause { get; set; } = string.Empty;

    /// <summary>Human-readable expansion of <see cref="Cause"/>, for the dashboard row.</summary>
    [JsonPropertyName("reason")]
    public string Reason { get; set; } = string.Empty;

    /// <summary>
    /// Configured workers that cannot output <see cref="Token"/>. Populated even when the operator
    /// also disabled the offer, so both halves of the reason stay visible.
    /// </summary>
    [JsonPropertyName("lacking")]
    public IReadOnlyList<string> Lacking { get; set; } = [];
}

/// <summary>One configured worker: its last-known capabilities and its current load.</summary>
public sealed class WorkerStatus
{
    /// <summary>The worker's identity in sync's map (usually its address).</summary>
    [JsonPropertyName("name")]
    public string Name { get; set; } = string.Empty;

    /// <summary>Where to reach it, e.g. <c>10.42.0.7:9901</c>.</summary>
    [JsonPropertyName("addr")]
    public string Addr { get; set; } = string.Empty;

    /// <summary>How sync found it this cycle: <c>dns</c>, <c>static</c> or <c>persisted</c>.</summary>
    [JsonPropertyName("source")]
    public string Source { get; set; } = string.Empty;

    /// <summary>The card class the agent reported, e.g. <c>qsv</c>.</summary>
    [JsonPropertyName("kind")]
    public string Kind { get; set; } = string.Empty;

    /// <summary>
    /// <c>up</c>, <c>down</c> or <c>never_seen</c>. There is no <c>draining</c>: the agent's Caps
    /// message has no draining field, so a worker shutting down reads as down (see
    /// <see cref="PoolStatus.Notes"/>).
    /// </summary>
    [JsonPropertyName("state")]
    public string State { get; set; } = string.Empty;

    /// <summary>Output tokens this worker can produce.</summary>
    [JsonPropertyName("outputs")]
    public IReadOnlyList<string> Outputs { get; set; } = [];

    /// <summary>Units this worker can serve at once.</summary>
    [JsonPropertyName("capacity_units")]
    public double CapacityUnits { get; set; }

    /// <summary>Units it is currently serving.</summary>
    [JsonPropertyName("units_used")]
    public double UnitsUsed { get; set; }

    /// <summary>Capacity minus load, floored at zero.</summary>
    [JsonPropertyName("units_free")]
    public double UnitsFree { get; set; }

    /// <summary>When the worker last answered Hello. 0 = never.</summary>
    [JsonPropertyName("seen_unix")]
    public long SeenUnix { get; set; }

    /// <summary>
    /// Seconds since <see cref="SeenUnix"/>, or <c>-1</c> for a worker that has never answered —
    /// deliberately not 0, so "never seen" cannot read as "seen just now".
    /// </summary>
    [JsonPropertyName("age_seconds")]
    public long AgeSeconds { get; set; }
}

/// <summary>Pool-wide unit totals, over the live workers only.</summary>
public sealed class PoolTotals
{
    /// <summary>Total units of the live workers.</summary>
    [JsonPropertyName("capacity_units")]
    public double CapacityUnits { get; set; }

    /// <summary>Total units in use across the live workers.</summary>
    [JsonPropertyName("units_used")]
    public double UnitsUsed { get; set; }

    /// <summary>Could the pool absorb losing its single biggest live card right now?</summary>
    [JsonPropertyName("survivable")]
    public bool Survivable { get; set; }
}

/// <summary>
/// The proxy's failure body, returned by our own controller (not sync) when <c>/status</c> could
/// not be read. Kept next to the wire format because it is the other half of the page's contract.
/// </summary>
public sealed class StatusUnavailable
{
    /// <summary>Why the pool view is empty: unreachable, timed out, non-200, unparseable, wrong schema.</summary>
    [JsonPropertyName("error")]
    public string Error { get; set; } = string.Empty;

    /// <summary>The sync base URL that was tried, so the operator can see which address is configured.</summary>
    [JsonPropertyName("sync_base_url")]
    public string SyncBaseUrl { get; set; } = string.Empty;

    /// <summary>When the attempt was made.</summary>
    [JsonPropertyName("checked_unix")]
    public long CheckedUnix { get; set; }
}

/// <summary>The pure half of the client: the JSON names, the options and the parse/serialize pair.</summary>
public static class StatusContract
{
    /// <summary>The only <c>/status</c> schema version this build can render.</summary>
    public const int SupportedSchema = 1;

    /// <summary>The path appended to the configured base URL.</summary>
    public const string StatusPath = "/status";

    /// <summary>
    /// Parsing options. Names come from the attributes, so no naming policy is applied; unknown
    /// fields are ignored rather than throwing, because sync adding a field is not a breaking
    /// change for a read-only view.
    /// </summary>
    public static readonly JsonSerializerOptions Options = new()
    {
        PropertyNamingPolicy = null,
        PropertyNameCaseInsensitive = false,
        ReadCommentHandling = JsonCommentHandling.Disallow,
        AllowTrailingCommas = false,
    };

    /// <summary>Parse a <c>/status</c> body. Throws <see cref="JsonException"/> on a malformed document.</summary>
    public static PoolStatus Parse(string json)
        => JsonSerializer.Deserialize<PoolStatus>(json, Options)
           ?? throw new JsonException("/status body deserialized to null");

    /// <summary>Serialize a document back to the wire shape (used by the proxy's response).</summary>
    public static string Serialize(PoolStatus status)
        => JsonSerializer.Serialize(status, Options);
}
