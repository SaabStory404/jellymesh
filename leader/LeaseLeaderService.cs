using System.Globalization;
using System.Net;
using System.Net.Http.Headers;
using System.Net.Security;
using System.Security.Cryptography.X509Certificates;
using System.Text;
using System.Text.Json.Nodes;
using Jellyfin.Data.Events;
using MediaBrowser.Controller.Library;
using MediaBrowser.Model.Tasks;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging;

namespace JellyMesh.Leader;

/// <summary>
/// Several Jellyfin replicas share one database; exactly one of them — the holder of a Kubernetes
/// Lease — runs scheduled tasks (library scans, cleanup, trickplay, plugin tasks) and the library
/// watcher. A task that starts on a follower (its own schedule, or "Scan library" pressed while the
/// browser happens to be routed to it) is cancelled there and forwarded to the holder through an
/// annotation on the same Lease; the holder runs it once. If the holder dies its lease expires and a
/// follower takes over. Outside Kubernetes there is nothing to share, so the node is the leader.
/// </summary>
/// <remarks>
/// Env: JELLYMESH_LEASE (Lease name, default "jellyfin-tasks"), JELLYMESH_LEASE_SECONDS (default 15).
/// The pod's ServiceAccount needs get/create/update/patch on leases in its namespace. The Lease is
/// Kubernetes' own coordination object: no second data store.
/// </remarks>
public sealed class LeaseLeaderService : IHostedService, IDisposable
{
    private const string SaDir = "/var/run/secrets/kubernetes.io/serviceaccount";
    private const string RunTaskAnnotation = "jellymesh.io/run-task";

    private readonly ITaskManager _taskManager;
    private readonly ILibraryMonitor _libraryMonitor;
    private readonly ILogger<LeaseLeaderService> _logger;
    private readonly CancellationTokenSource _cts = new();
    private readonly string _identity = Environment.GetEnvironmentVariable("HOSTNAME") ?? Environment.MachineName;
    private readonly string _leaseName = Environment.GetEnvironmentVariable("JELLYMESH_LEASE") ?? "jellyfin-tasks";
    private readonly int _leaseSeconds = int.TryParse(Environment.GetEnvironmentVariable("JELLYMESH_LEASE_SECONDS"), out var s) ? s : 15;
    private readonly bool _inKubernetes = Environment.GetEnvironmentVariable("KUBERNETES_SERVICE_HOST") is { Length: > 0 } && File.Exists(SaDir + "/token");
    private HttpClient? _http;
    private string _leasesUrl = string.Empty;
    private string _leaseUrl = string.Empty;
    private string _lastHandledRequest = string.Empty;

    public LeaseLeaderService(ITaskManager taskManager, ILibraryMonitor libraryMonitor, ILogger<LeaseLeaderService> logger)
    {
        _taskManager = taskManager;
        _libraryMonitor = libraryMonitor;
        _logger = logger;
    }

    public bool IsLeader { get; private set; }

    public Task StartAsync(CancellationToken cancellationToken)
    {
        if (!_inKubernetes)
        {
            IsLeader = true;
            _logger.LogInformation("JellyMesh Leader: not in Kubernetes, this node runs scheduled tasks");
            return Task.CompletedTask;
        }

        var ns = File.ReadAllText(SaDir + "/namespace").Trim();
        var host = Environment.GetEnvironmentVariable("KUBERNETES_SERVICE_HOST");
        var port = Environment.GetEnvironmentVariable("KUBERNETES_SERVICE_PORT") ?? "443";
        var ca = X509Certificate2.CreateFromPemFile(SaDir + "/ca.crt");
        var handler = new HttpClientHandler
        {
            // Trust the cluster CA from the ServiceAccount mount (not the system store).
            ServerCertificateCustomValidationCallback = (_, cert, chain, errors) =>
            {
                if (errors == SslPolicyErrors.None)
                {
                    return true;
                }

                if (cert is null || chain is null)
                {
                    return false;
                }

                chain.ChainPolicy.TrustMode = X509ChainTrustMode.CustomRootTrust;
                chain.ChainPolicy.CustomTrustStore.Add(ca);
                chain.ChainPolicy.RevocationMode = X509RevocationMode.NoCheck;
                return chain.Build(cert);
            },
        };
        _http = new HttpClient(handler) { BaseAddress = new Uri($"https://{host}:{port}"), Timeout = TimeSpan.FromSeconds(5) };
        _leasesUrl = $"/apis/coordination.k8s.io/v1/namespaces/{ns}/leases";
        _leaseUrl = $"{_leasesUrl}/{_leaseName}";

        _taskManager.TaskExecuting += OnTaskExecuting;
        _ = Task.Run(() => Loop(ns, _cts.Token));
        _logger.LogInformation("JellyMesh Leader: identity {Identity}, lease {Ns}/{Lease} ({Seconds} s)", _identity, ns, _leaseName, _leaseSeconds);
        return Task.CompletedTask;
    }

    public async Task StopAsync(CancellationToken cancellationToken)
    {
        _taskManager.TaskExecuting -= OnTaskExecuting;
        await _cts.CancelAsync().ConfigureAwait(false);
        if (IsLeader && _http is not null)
        {
            // Hand over at once instead of making the next leader wait for expiry.
            try
            {
                SetToken();
                await Patch(new JsonObject { ["spec"] = new JsonObject { ["holderIdentity"] = null } }, CancellationToken.None).ConfigureAwait(false);
            }
            catch (Exception)
            {
                // expiry will do it
            }
        }
    }

    public void Dispose()
    {
        _cts.Dispose();
        _http?.Dispose();
    }

    private async Task Loop(string ns, CancellationToken ct)
    {
        var sinceStart = System.Diagnostics.Stopwatch.StartNew();
        var watcherStopped = false;
        using var timer = new PeriodicTimer(TimeSpan.FromSeconds(Math.Max(1, _leaseSeconds / 5)));
        while (await timer.WaitForNextTickAsync(ct).ConfigureAwait(false))
        {
            try
            {
                var leader = await Tick(ns, ct).ConfigureAwait(false);
                if (leader != IsLeader)
                {
                    SetRole(leader);
                }
                else if (!leader && !watcherStopped && sinceStart.Elapsed > TimeSpan.FromSeconds(30))
                {
                    // Jellyfin starts the library monitor itself during startup, after plugins load.
                    _libraryMonitor.Stop();
                    watcherStopped = true;
                }
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                _logger.LogWarning("JellyMesh Leader: lease check failed ({Error})", ex.Message);
                if (IsLeader)
                {
                    // Cannot renew: step down before the lease can expire under us.
                    SetRole(false);
                }
            }
        }
    }

    // One election round. Returns whether this replica holds the lease afterwards.
    private async Task<bool> Tick(string ns, CancellationToken ct)
    {
        SetToken(); // projected ServiceAccount tokens rotate
        var now = DateTime.UtcNow;
        using var get = await _http!.GetAsync(_leaseUrl, ct).ConfigureAwait(false);
        if (get.StatusCode == HttpStatusCode.NotFound)
        {
            var created = new JsonObject
            {
                ["apiVersion"] = "coordination.k8s.io/v1",
                ["kind"] = "Lease",
                ["metadata"] = new JsonObject { ["name"] = _leaseName, ["namespace"] = ns },
                ["spec"] = Spec(now, now),
            };
            using var post = await _http.PostAsync(_leasesUrl, Json(created), ct).ConfigureAwait(false);
            return post.IsSuccessStatusCode;
        }

        get.EnsureSuccessStatusCode();
        var lease = JsonNode.Parse(await get.Content.ReadAsStringAsync(ct).ConfigureAwait(false))!.AsObject();
        var spec = lease["spec"] as JsonObject ?? new JsonObject();
        var holder = (string?)spec["holderIdentity"];
        var renew = ParseTime((string?)spec["renewTime"]);
        var duration = (int?)spec["leaseDurationSeconds"] ?? _leaseSeconds;
        var expired = string.IsNullOrEmpty(holder) || renew is null || renew.Value.AddSeconds(duration) < now;

        if (holder != _identity && !expired)
        {
            return false;
        }

        // Renew, or take over an expired/released lease. The resourceVersion carried in the PUT makes
        // a concurrent takeover by another replica fail with 409 instead of both believing they hold it.
        var acquire = holder == _identity ? ParseTime((string?)spec["acquireTime"]) ?? now : now;
        lease["spec"] = Spec(acquire, now);
        using var put = await _http.PutAsync(_leaseUrl, Json(lease), ct).ConfigureAwait(false);
        if (!put.IsSuccessStatusCode)
        {
            return false;
        }

        // Forwarded task requests ride on the lease: "<task key>|<unix ms>".
        var request = (string?)lease["metadata"]?["annotations"]?[RunTaskAnnotation];
        if (!string.IsNullOrEmpty(request) && request != _lastHandledRequest)
        {
            _lastHandledRequest = request;
            RunForwarded(request.Split('|')[0]);
        }

        return true;
    }

    private JsonObject Spec(DateTime acquire, DateTime renew) => new()
    {
        ["holderIdentity"] = _identity,
        ["leaseDurationSeconds"] = _leaseSeconds,
        ["acquireTime"] = MicroTime(acquire),
        ["renewTime"] = MicroTime(renew),
    };

    private void SetRole(bool leader)
    {
        IsLeader = leader;
        _logger.LogInformation("JellyMesh Leader: {Identity} is now {Role}", _identity, leader ? "LEADER" : "follower");
        if (leader)
        {
            _libraryMonitor.Start();
        }
        else
        {
            _libraryMonitor.Stop();
        }
    }

    private void OnTaskExecuting(object? sender, GenericEventArgs<IScheduledTaskWorker> e)
    {
        if (IsLeader)
        {
            return;
        }

        var worker = e.Argument;
        _taskManager.Cancel(worker);
        var key = worker.ScheduledTask.Key;
        _logger.LogInformation("JellyMesh Leader: follower {Identity} cancelled '{Task}' and forwarded it to the leader", _identity, worker.Name);
        var patch = new JsonObject
        {
            ["metadata"] = new JsonObject
            {
                ["annotations"] = new JsonObject
                {
                    [RunTaskAnnotation] = key + "|" + DateTimeOffset.UtcNow.ToUnixTimeMilliseconds().ToString(CultureInfo.InvariantCulture),
                },
            },
        };
        _ = Patch(patch, CancellationToken.None);
    }

    private void RunForwarded(string key)
    {
        var worker = _taskManager.ScheduledTasks.FirstOrDefault(w => w.ScheduledTask.Key == key);
        if (worker is null || worker.State != TaskState.Idle)
        {
            _logger.LogInformation("JellyMesh Leader: forwarded task {Key} skipped (state {State})", key, worker?.State);
            return;
        }

        _logger.LogInformation("JellyMesh Leader: running forwarded task '{Task}'", worker.Name);
        _ = _taskManager.Execute(worker, new TaskOptions());
    }

    private async Task Patch(JsonObject body, CancellationToken ct)
    {
        using var content = new StringContent(body.ToJsonString(), Encoding.UTF8);
        content.Headers.ContentType = new MediaTypeHeaderValue("application/merge-patch+json");
        using var r = await _http!.PatchAsync(_leaseUrl, content, ct).ConfigureAwait(false);
        if (!r.IsSuccessStatusCode)
        {
            _logger.LogWarning("JellyMesh Leader: lease patch failed ({Status})", (int)r.StatusCode);
        }
    }

    private void SetToken()
    {
        var token = File.ReadAllText(SaDir + "/token").Trim();
        _http!.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", token);
    }

    private static StringContent Json(JsonNode node) => new(node.ToJsonString(), Encoding.UTF8, "application/json");

    private static string MicroTime(DateTime t) => t.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'", CultureInfo.InvariantCulture);

    private static DateTime? ParseTime(string? s) =>
        DateTime.TryParse(s, CultureInfo.InvariantCulture, DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var t) ? t : null;
}
