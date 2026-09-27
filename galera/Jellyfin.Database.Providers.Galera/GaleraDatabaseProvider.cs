using System.Globalization;
using Jellyfin.Database.Implementations;
using Jellyfin.Database.Implementations.DbConfiguration;
using MediaBrowser.Common.Configuration;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Diagnostics;
using Microsoft.Extensions.Logging;

namespace Jellyfin.Database.Providers.Galera;

/// <summary>
/// Runs Jellyfin on MySQL 8 / Percona XtraDB Cluster (Galera): every node keeps a full copy of the
/// database in memory and any node can write. Deliberately thin — no query caching or rewriting
/// interceptors (see docs/jellymesh-db-options.md for why).
/// </summary>
[JellyfinDatabaseProviderKey("Jellyfin-Galera")]
public sealed class GaleraDatabaseProvider : IJellyfinDatabaseProvider
{
    private readonly ILogger<GaleraDatabaseProvider> _logger;

    /// <summary>Initializes a new instance of the <see cref="GaleraDatabaseProvider"/> class.</summary>
    /// <param name="applicationPaths">Jellyfin paths (unused; the constructor shape matches Jellyfin's providers).</param>
    /// <param name="logger">Logger.</param>
    public GaleraDatabaseProvider(IApplicationPaths applicationPaths, ILogger<GaleraDatabaseProvider> logger)
    {
        _logger = logger;
    }

    /// <inheritdoc/>
    public IDbContextFactory<JellyfinDbContext>? DbContextFactory { get; set; }

    /// <inheritdoc/>
    public void Initialise(DbContextOptionsBuilder options, DatabaseConfigurationOptions databaseConfiguration)
    {
        var connectionString = databaseConfiguration.CustomProviderOptions?.ConnectionString
            ?? throw new InvalidOperationException("database.xml must set CustomProviderOptions/ConnectionString for Jellyfin-Galera");
        _logger.LogInformation("Galera provider: {Server}", RedactPassword(connectionString));
        // String lists bound as JSON_TABLE() compare against utf8mb4_bin columns (GaleraModelConvention).
        AppContext.SetData("Pomelo.EntityFrameworkCore.MySql.JsonTableStringCollation", GaleraModelConvention.Collation);

        // Fixed server version: AutoDetect would open a connection at startup on every context.
        options
            .UseMySql(connectionString, new MySqlServerVersion(new Version(8, 4, 0)), mysql => mysql
                .MigrationsAssembly(GetType().Assembly.GetName().Name)
                // Jellyfin filters on captured Guid[]s (/UserViews: ids.Contains(x.ItemId)); without this
                // Pomelo refuses to translate them. The model's own list columns keep their explicit
                // JSON converters (GaleraModelConvention), so this changes query translation only.
                .EnablePrimitiveCollectionsSupport())
            .ConfigureWarnings(w => w.Ignore(RelationalEventId.NonTransactionalMigrationOperationWarning)
                .Ignore(RelationalEventId.MultipleCollectionIncludeWarning));
    }

    /// <inheritdoc/>
    public void OnModelCreating(ModelBuilder modelBuilder)
    {
    }

    /// <inheritdoc/>
    public void ConfigureConventions(ModelConfigurationBuilder configurationBuilder)
    {
        configurationBuilder.Conventions.Add(_ => new GaleraModelConvention());
    }

    /// <inheritdoc/>
    public async Task RunScheduledOptimisation(CancellationToken cancellationToken)
    {
        if (DbContextFactory is null)
        {
            return;
        }

        var context = await DbContextFactory.CreateDbContextAsync(cancellationToken).ConfigureAwait(false);
        await using (context.ConfigureAwait(false))
        {
            foreach (var table in context.Model.GetEntityTypes().Select(e => e.GetTableName()).Where(t => t is not null).Distinct())
            {
                await context.Database.ExecuteSqlRawAsync($"ANALYZE TABLE `{table}`", cancellationToken).ConfigureAwait(false);
            }
        }
    }

    /// <inheritdoc/>
    public Task RunShutdownTask(CancellationToken cancellationToken) => Task.CompletedTask;

    /// <inheritdoc/>
    /// <remarks>
    /// Jellyfin asks for a backup before every migration run. A Galera cluster is backed up by its
    /// operator (xtrabackup), not by one Jellyfin node, so this only records a key.
    /// It is also the one hook that runs before migrations, so it sets the database default to
    /// utf8mb4_bin: Oracle's SQL generator drops per-column collations (MEASURED, 10.0.9), and
    /// tables created afterwards inherit the database default instead.
    /// </remarks>
    public async Task<string> MigrationBackupFast(CancellationToken cancellationToken)
    {
        var key = DateTime.UtcNow.ToString("yyyyMMddHHmmss", CultureInfo.InvariantCulture);
        if (DbContextFactory is not null)
        {
            var context = await DbContextFactory.CreateDbContextAsync(cancellationToken).ConfigureAwait(false);
            await using (context.ConfigureAwait(false))
            {
                await context.Database.ExecuteSqlRawAsync(
                    $"ALTER DATABASE CHARACTER SET utf8mb4 COLLATE {GaleraModelConvention.Collation}", cancellationToken).ConfigureAwait(false);
            }
        }

        _logger.LogWarning("Galera provider: pre-migration backup {Key} is not taken here; rely on the cluster's own backups", key);
        return key;
    }

    /// <inheritdoc/>
    public Task RestoreBackupFast(string key, CancellationToken cancellationToken)
    {
        _logger.LogCritical("Galera provider cannot restore backup {Key}; restore the cluster from its own backups", key);
        return Task.CompletedTask;
    }

    /// <inheritdoc/>
    public Task DeleteBackup(string key) => Task.CompletedTask;

    /// <inheritdoc/>
    public async Task PurgeDatabase(JellyfinDbContext dbContext, IEnumerable<string>? tableNames)
    {
        ArgumentNullException.ThrowIfNull(tableNames);
        var sql = "SET FOREIGN_KEY_CHECKS = 0;\n"
            + string.Join('\n', tableNames.Select(t => $"DELETE FROM `{t}`;"))
            + "\nSET FOREIGN_KEY_CHECKS = 1;";
        await dbContext.Database.ExecuteSqlRawAsync(sql).ConfigureAwait(false);
    }

    private static string RedactPassword(string connectionString) =>
        string.Join(';', connectionString.Split(';').Select(p =>
            p.TrimStart().StartsWith("password", StringComparison.OrdinalIgnoreCase) || p.TrimStart().StartsWith("pwd", StringComparison.OrdinalIgnoreCase)
                ? p[..(p.IndexOf('=') + 1)] + "*****"
                : p));
}
