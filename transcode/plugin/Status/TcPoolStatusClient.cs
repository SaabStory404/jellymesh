using System.Net;
using System.Text.Json;
using JellyMesh.TranscodePool.Configuration;
using Microsoft.Extensions.Logging;

namespace JellyMesh.TranscodePool.Status;

/// <summary>
/// Reads tcpool-sync's <c>/status</c> endpoint (crates/sync/src/status.rs) over HTTP and hands back
/// the parsed document.
/// </summary>
/// <remarks>
/// <para>
/// The base URL comes from the plugin configuration and is read on every call, so an operator who
/// repoints the plugin at another sync does not have to restart Jellyfin.
/// </para>
/// <para>
/// Every failure — bad URL, DNS, refused connection, timeout, non-200, malformed JSON, a schema
/// this build does not know — becomes a <see cref="StatusFetch.Error"/> string. The dashboard page
/// is a status display, so "why is the pool view empty" is the one thing it must never lose; an
/// exception that reaches MVC's problem-details handler loses it.
/// </para>
/// <para>
/// The handler is injectable (<see cref="HttpMessageHandler"/>) so the mapping above can be tested
/// against a stub handler later, with no sync and no listener.
/// </para>
/// </remarks>
public sealed class TcPoolStatusClient : IStatusSource, IDisposable
{
    private readonly Func<PluginConfiguration> _configuration;
    private readonly ILogger<TcPoolStatusClient> _logger;
    private readonly HttpClient _http;

    /// <summary>
    /// Initializes a new instance of the <see cref="TcPoolStatusClient"/> class.
    /// </summary>
    /// <param name="configuration">Supplies the current plugin configuration on every call.</param>
    /// <param name="logger">The logger.</param>
    /// <param name="handler">A message handler; the default pooled one is used when null.</param>
    public TcPoolStatusClient(
        Func<PluginConfiguration> configuration,
        ILogger<TcPoolStatusClient> logger,
        HttpMessageHandler? handler = null)
    {
        _configuration = configuration;
        _logger = logger;
        _http = handler is null ? new HttpClient() : new HttpClient(handler);

        // The deadline is per call, from configuration (sync also sets its own; the shorter wins),
        // so there is no artificial ceiling baked into the client.
        _http.Timeout = Timeout.InfiniteTimeSpan;
    }

    /// <inheritdoc />
    public async Task<StatusFetch> GetStatusAsync(CancellationToken cancellationToken)
    {
        var config = _configuration();
        var checkedUnix = DateTimeOffset.UtcNow.ToUnixTimeSeconds();
        var baseUrl = (config.SyncBaseUrl ?? string.Empty).Trim().TrimEnd('/');

        if (!Uri.TryCreate(baseUrl, UriKind.Absolute, out var root)
            || (root.Scheme != Uri.UriSchemeHttp && root.Scheme != Uri.UriSchemeHttps))
        {
            return new StatusFetch(
                null,
                $"the configured sync base URL '{baseUrl}' is not an absolute http(s) URL",
                baseUrl,
                checkedUnix);
        }

        var url = new Uri(root, StatusContract.StatusPath);
        var timeout = TimeSpan.FromSeconds(config.TimeoutSeconds > 0 ? config.TimeoutSeconds : 5);
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        deadline.CancelAfter(timeout);

        string json;
        try
        {
            using var response = await _http.GetAsync(url, HttpCompletionOption.ResponseHeadersRead, deadline.Token)
                .ConfigureAwait(false);

            if (response.StatusCode != HttpStatusCode.OK)
            {
                return Fail($"tcpool-sync at {baseUrl} answered {StatusLine(response.StatusCode)}", baseUrl, checkedUnix);
            }

            json = await response.Content.ReadAsStringAsync(deadline.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            // Our deadline, not the caller's: sync accepted the connection and then stopped talking.
            return Fail($"tcpool-sync at {baseUrl} did not answer within {timeout.TotalSeconds:0.#} s", baseUrl, checkedUnix);
        }
        catch (HttpRequestException ex)
        {
            // DNS failure, refused connection, TLS, reset mid-body -- all "the pool is not there".
            return Fail($"tcpool-sync at {baseUrl} is unreachable ({ex.Message})", baseUrl, checkedUnix);
        }

        PoolStatus status;
        try
        {
            status = StatusContract.Parse(json);
        }
        catch (JsonException ex)
        {
            return Fail($"/status from {baseUrl} does not match the documented shape ({ex.Message})", baseUrl, checkedUnix);
        }

        if (status.Schema != StatusContract.SupportedSchema)
        {
            // Deliberately loud: the page would be showing stale or wrong columns otherwise.
            return Fail(
                $"/status from {baseUrl} reports schema {status.Schema}; this plugin understands {StatusContract.SupportedSchema}",
                baseUrl,
                checkedUnix);
        }

        return new StatusFetch(status, null, baseUrl, checkedUnix);
    }

    /// <inheritdoc />
    public void Dispose() => _http.Dispose();

    private StatusFetch Fail(string error, string baseUrl, long checkedUnix)
    {
        _logger.LogWarning("Transcode Pool: {Error}", error);
        return new StatusFetch(null, error, baseUrl, checkedUnix);
    }

    private static string StatusLine(HttpStatusCode code) => $"HTTP {(int)code} {code}";
}
