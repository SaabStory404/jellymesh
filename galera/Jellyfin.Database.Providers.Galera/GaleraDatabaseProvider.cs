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
public sealed partial class GaleraDatabaseProvider : IJellyfinDatabaseProvider
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

        // The password can come from the environment (a Kubernetes Secret) instead of database.xml,
        // which lives in the config directory and so in every backup of it.
        var password = Environment.GetEnvironmentVariable("JELLYMESH_DB_PASSWORD");
        if (!string.IsNullOrEmpty(password))
        {
            connectionString = new MySqlConnector.MySqlConnectionStringBuilder(connectionString) { Password = password }.ConnectionString;
        }

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
            // /health: CanConnect runs SELECT 1 on the pooled connection, not a new unpooled one per probe.
            .ReplaceService<Microsoft.EntityFrameworkCore.Storage.IRelationalDatabaseCreator, GaleraDatabaseCreator>()
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
    /// <remarks>
    /// FOREIGN_KEY_CHECKS is session state. With <c>ConnectionReset=false</c> a pooled connection keeps it
    /// for its next user, so the checks are switched back on in a finally on the same (explicitly opened)
    /// connection, and if even that fails the pool is cleared so this session is never handed out again.
    /// </remarks>
    public async Task PurgeDatabase(JellyfinDbContext dbContext, IEnumerable<string>? tableNames)
    {
        ArgumentNullException.ThrowIfNull(dbContext);
        ArgumentNullException.ThrowIfNull(tableNames);
        var database = dbContext.Database;
        await database.OpenConnectionAsync().ConfigureAwait(false);
        try
        {
            await PurgeAsync(
                sql => database.ExecuteSqlRawAsync(sql),
                tableNames,
                () => MySqlConnector.MySqlConnection.ClearPool((MySqlConnector.MySqlConnection)database.GetDbConnection())).ConfigureAwait(false);
        }
        finally
        {
            await database.CloseConnectionAsync().ConfigureAwait(false);
        }
    }

    /// <summary>
    /// Deletes every row of <paramref name="tableNames"/> with foreign-key checks off, and always turns
    /// them back on afterwards. All statements must run on one connection.
    /// </summary>
    /// <param name="execute">Runs one SQL statement on the connection.</param>
    /// <param name="tableNames">Tables to empty.</param>
    /// <param name="discardConnection">Called when the checks could not be restored: the connection must not be reused.</param>
    /// <returns>A task.</returns>
    internal static async Task PurgeAsync(Func<string, Task> execute, IEnumerable<string> tableNames, Action discardConnection)
    {
        await execute("SET FOREIGN_KEY_CHECKS = 0").ConfigureAwait(false);
        try
        {
            var deletes = string.Join('\n', tableNames.Select(t => $"DELETE FROM `{t}`;"));
            if (deletes.Length > 0)
            {
                await execute(deletes).ConfigureAwait(false);
            }
        }
        finally
        {
            try
            {
                await execute("SET FOREIGN_KEY_CHECKS = 1").ConfigureAwait(false);
            }
            catch
            {
                discardConnection();
                throw;
            }
        }
    }

    /// <summary>
    /// Masks the password in a MySQL connection string for logging. Parsed with the connection-string
    /// builder so a quoted password containing <c>;</c> (<c>Password="a;b"</c>) is masked whole; the
    /// regex fallback (malformed strings the builder rejects) also treats quoted values as one token.
    /// </summary>
    /// <param name="connectionString">Connection string, possibly with a password.</param>
    /// <returns>The connection string with any password replaced by <c>*****</c>.</returns>
    internal static string RedactPassword(string connectionString)
    {
        try
        {
            var builder = new MySqlConnector.MySqlConnectionStringBuilder(connectionString);
            if (!string.IsNullOrEmpty(builder.Password))
            {
                builder.Password = "*****";
            }

            return builder.ConnectionString;
        }
        catch (Exception ex) when (ex is ArgumentException or FormatException or InvalidOperationException)
        {
            // Logging must never be what fails startup; a bad value (Port=abc) surfaces at connect.
            return PasswordPattern().Replace(connectionString, "$1*****");
        }
    }

    // key = "double-quoted with "" escapes" | 'single-quoted with '' escapes' | bare to next ';'.
    [System.Text.RegularExpressions.GeneratedRegex("""((?:^|;)\s*(?:password|pwd)\s*=\s*)(?:"(?:[^"]|"")*"?|'(?:[^']|'')*'?|[^;]*)""", System.Text.RegularExpressions.RegexOptions.IgnoreCase)]
    private static partial System.Text.RegularExpressions.Regex PasswordPattern();
}
