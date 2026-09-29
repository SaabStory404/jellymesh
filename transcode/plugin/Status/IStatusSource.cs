namespace JellyMesh.TranscodePool.Status;

/// <summary>
/// The pool status, as the plugin sees it. Exactly one implementation today
/// (<see cref="TcPoolStatusClient"/>, over sync's <c>/status</c> HTTP endpoint), and everything
/// else depends on this interface rather than on that client — so the day sync publishes the pool
/// state somewhere else (a gRPC call, a file on the shared scratch, an event stream), only the
/// registration in <c>ServiceRegistrator</c> changes.
/// </summary>
public interface IStatusSource
{
    /// <summary>
    /// Read the current pool status. Never throws for an unreachable or unparseable pool: the
    /// failure comes back in <see cref="StatusFetch.Error"/> so the caller can render "the pool
    /// view is unavailable, and here is why" instead of a stack trace.
    /// </summary>
    Task<StatusFetch> GetStatusAsync(CancellationToken cancellationToken);
}

/// <summary>One read of the pool status: either a document, or the reason there isn't one.</summary>
/// <param name="Status">The parsed document, or null on failure.</param>
/// <param name="Error">Why the read failed, in operator-readable words, or null on success.</param>
/// <param name="SyncBaseUrl">The base URL that was tried, for the error banner.</param>
/// <param name="CheckedUnix">When the attempt was made (unix seconds).</param>
public sealed record StatusFetch(PoolStatus? Status, string? Error, string SyncBaseUrl, long CheckedUnix)
{
    /// <summary>True when <see cref="Status"/> was read and parsed.</summary>
    public bool Ok => Status is not null;
}
