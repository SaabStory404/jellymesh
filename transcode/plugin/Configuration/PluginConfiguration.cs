using MediaBrowser.Model.Plugins;

namespace JellyMesh.TranscodePool.Configuration;

/// <summary>
/// Where tcpool-sync's <c>/status</c> view lives. Serialized to
/// <c>JellyMesh.TranscodePool.xml</c> in the plugin configuration folder, so it survives restarts
/// and is editable from the plugin's own Jellyfin configuration page.
/// </summary>
public class PluginConfiguration : BasePluginConfiguration
{
    /// <summary>
    /// The default sync address. Deploy/k8s/30-service.yaml gives tcpool-sync its metrics port
    /// (9904) on the <c>media</c> namespace, and <c>/status</c> is served on that same port next to
    /// <c>/metrics</c> (crates/sync/src/metrics.rs), so this is the in-cluster name and port.
    /// </summary>
    public const string DefaultSyncBaseUrl = "http://tcpool-sync.media:9904";

    /// <summary>
    /// Gets or sets the base URL of tcpool-sync, without a trailing slash. The client appends
    /// <c>/status</c>. Read on every refresh, so editing it takes effect without a restart.
    /// </summary>
    public string SyncBaseUrl { get; set; } = DefaultSyncBaseUrl;

    /// <summary>
    /// Gets or sets how long to wait for <c>/status</c> before giving up and reporting the pool as
    /// unreachable.
    /// </summary>
    public int TimeoutSeconds { get; set; } = 5;
}
