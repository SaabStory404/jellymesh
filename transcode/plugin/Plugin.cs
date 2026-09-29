using JellyMesh.TranscodePool.Configuration;
using JellyMesh.TranscodePool.Status;
using MediaBrowser.Common.Configuration;
using MediaBrowser.Common.Plugins;
using MediaBrowser.Controller;
using MediaBrowser.Controller.Plugins;
using MediaBrowser.Model.Plugins;
using MediaBrowser.Model.Serialization;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging;

namespace JellyMesh.TranscodePool;

/// <summary>
/// The "Transcode Pool" plugin: a read-only operator view of the GPU transcode pool, in the
/// dashboard, next to the Grafana board and Kuma (PLAN §11).
/// </summary>
/// <remarks>
/// Jellyfin still runs with hardware acceleration "none" and the shim still sends every HLS
/// transcode to the pool; this plugin changes nothing about that. It reports what the pool is
/// doing: which workers are up, what each can output, their units used against capacity, which
/// codecs Jellyfin is offering and why, and the pool's emergency state.
/// <para>
/// Controls (drain a worker, HEVC/AV1 intent) are deliberately absent — PLAN §11 puts them in P4,
/// behind sync's admin client cert and a new agent RPC, and a read-only page cannot drift the pool
/// by accident.
/// </para>
/// </remarks>
public class Plugin : BasePlugin<PluginConfiguration>, IHasWebPages
{
    /// <summary>
    /// The logical name of the embedded dashboard page, matching the LogicalName in the csproj.
    /// Jellyfin resolves it with <c>Assembly.GetManifestResourceStream</c> and serves it as the
    /// page body.
    /// </summary>
    internal const string StatusPageResource = "JellyMesh.TranscodePool.Configuration.statusPage.html";

    /// <summary>
    /// Initializes a new instance of the <see cref="Plugin"/> class.
    /// </summary>
    /// <param name="applicationPaths">The application paths.</param>
    /// <param name="xmlSerializer">The XML serializer.</param>
    public Plugin(IApplicationPaths applicationPaths, IXmlSerializer xmlSerializer)
        : base(applicationPaths, xmlSerializer)
    {
        Instance = this;
    }

    /// <summary>
    /// Gets the loaded plugin. Set in the constructor, which Jellyfin runs before it calls the
    /// service registrator; the registered client reads configuration through it so that a
    /// base-URL change takes effect without a restart.
    /// </summary>
    public static Plugin? Instance { get; private set; }

    /// <inheritdoc />
    public override string Name => "Transcode Pool";

    /// <inheritdoc />
    public override string Description =>
        "Read-only view of the GPU transcode pool: workers, capabilities, units, codec offers and emergency mode.";

    /// <inheritdoc />
    public override Guid Id => Guid.Parse("7c1d5a34-2f6b-4c8e-9d31-5a7e0b9d4c22");

    /// <inheritdoc />
    public IEnumerable<PluginPageInfo> GetPages()
    {
        return
        [
            new PluginPageInfo
            {
                Name = "tcpool",
                DisplayName = "Transcode Pool",
                EmbeddedResourcePath = StatusPageResource,

                // The pool is an operator view, not a menu item: it belongs with the other plugin
                // pages under Dashboard -> Plugins, where an admin already looks.
                EnableInMainMenu = false,
            },
        ];
    }
}

/// <summary>
/// Wires the plugin's own services into the server container. Only the status source is needed:
/// Jellyfin already registers every plugin assembly as an MVC application part, and
/// <c>AddControllersAsServices</c> resolves <c>TcPoolController</c> from this container — so the
/// controller's constructor dependencies have to exist here.
/// </summary>
public class ServiceRegistrator : IPluginServiceRegistrator
{
    /// <inheritdoc />
    public void RegisterServices(IServiceCollection serviceCollection, IServerApplicationHost applicationHost)
    {
        // One client (and so one connection pool) for the process. Configuration is read per call,
        // which is why the plugin instance is captured rather than its current values.
        serviceCollection.AddSingleton<IStatusSource>(sp => new TcPoolStatusClient(
            () => Plugin.Instance?.Configuration ?? new PluginConfiguration(),
            sp.GetRequiredService<ILogger<TcPoolStatusClient>>()));
    }
}
