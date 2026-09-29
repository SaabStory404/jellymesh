using JellyMesh.TranscodePool.Status;
using MediaBrowser.Common.Api;
using Microsoft.AspNetCore.Authorization;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.Logging;

namespace JellyMesh.TranscodePool.Api;

/// <summary>
/// The one REST endpoint the dashboard page talks to: <c>GET /TcPool/Status</c>.
/// </summary>
/// <remarks>
/// <para>
/// Why a proxy instead of letting the page call sync directly: <c>tcpool-sync</c> is a sidecar of
/// the Jellyfin pod on a cluster-internal Service (deploy/k8s/30-service.yaml), so an operator's
/// browser cannot resolve <c>tcpool-sync.media</c>. Even when it can, sync has no CORS and no
/// Jellyfin authentication, and the plugin already holds the address in configuration. Jellyfin
/// registers every plugin assembly as an MVC application part
/// (Jellyfin.Server/Extensions/ApiServiceCollectionExtensions.cs), so the controller is found
/// without any extra registration.
/// </para>
/// <para>
/// Admin-only, matching the page itself: Jellyfin's own <c>/web/ConfigurationPages</c> is
/// administrator-only, and this shows pool capacity, worker addresses and scheduling state.
/// </para>
/// </remarks>
[ApiController]
[Route("TcPool")]
[Authorize(Policy = Policies.RequiresElevation)]
public sealed class TcPoolController : ControllerBase
{
    private readonly IStatusSource _statusSource;
    private readonly ILogger<TcPoolController> _logger;

    /// <summary>
    /// Initializes a new instance of the <see cref="TcPoolController"/> class.
    /// </summary>
    /// <param name="statusSource">Where the pool status comes from.</param>
    /// <param name="logger">The logger.</param>
    public TcPoolController(IStatusSource statusSource, ILogger<TcPoolController> logger)
    {
        _statusSource = statusSource;
        _logger = logger;
    }

    /// <summary>
    /// Reads the pool as sync last reconciled it.
    /// </summary>
    /// <param name="cancellationToken">The request's cancellation token.</param>
    /// <returns>
    /// 200 with sync's <c>/status</c> document, or 502 with the reason it could not be read. A 502
    /// rather than an empty 200: the page must not be able to mistake "sync is down" for "the pool
    /// is empty", which is the one reading that would make an operator act on nothing.
    /// </returns>
    [HttpGet("Status")]
    [Produces("application/json")]
    public async Task<ActionResult<PoolStatus>> GetStatus(CancellationToken cancellationToken)
    {
        var fetch = await _statusSource.GetStatusAsync(cancellationToken).ConfigureAwait(false);
        if (!fetch.Ok)
        {
            _logger.LogWarning("Transcode Pool: /TcPool/Status unavailable: {Error}", fetch.Error);
            return StatusCode(
                StatusCodes.Status502BadGateway,
                new StatusUnavailable
                {
                    Error = fetch.Error ?? "the pool status could not be read",
                    SyncBaseUrl = fetch.SyncBaseUrl,
                    CheckedUnix = fetch.CheckedUnix,
                });
        }

        return Ok(fetch.Status);
    }
}
