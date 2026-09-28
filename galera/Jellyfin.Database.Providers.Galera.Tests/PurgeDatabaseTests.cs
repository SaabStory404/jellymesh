using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.Logging.Abstractions;
using MySqlConnector;
using Xunit;

namespace Jellyfin.Database.Providers.Galera.Tests;

public class PurgeDatabaseTests
{
    private const string Off = "SET FOREIGN_KEY_CHECKS = 0";
    private const string On = "SET FOREIGN_KEY_CHECKS = 1";

    [Fact]
    public async Task Purge_DeletesWithChecksOff_ThenRestores()
    {
        var log = new List<string>();
        var discarded = false;

        await GaleraDatabaseProvider.PurgeAsync(s => { log.Add(s); return Task.CompletedTask; }, ["A", "B"], () => discarded = true);

        Assert.Equal([Off, "DELETE FROM `A`;\nDELETE FROM `B`;", On], log);
        Assert.False(discarded);
    }

    [Fact]
    public async Task Purge_DeleteFails_StillRestoresChecks()
    {
        var log = new List<string>();
        var discarded = false;

        var ex = await Assert.ThrowsAsync<InvalidOperationException>(() => GaleraDatabaseProvider.PurgeAsync(
            s =>
            {
                log.Add(s);
                return s.StartsWith("DELETE", StringComparison.Ordinal) ? throw new InvalidOperationException("delete failed") : Task.CompletedTask;
            },
            ["A"],
            () => discarded = true));

        Assert.Equal("delete failed", ex.Message);
        Assert.Equal(On, log[^1]);
        Assert.False(discarded);
    }

    [Fact]
    public async Task Purge_RestoreFails_DiscardsConnection()
    {
        var discarded = false;

        await Assert.ThrowsAsync<TimeoutException>(() => GaleraDatabaseProvider.PurgeAsync(
            s => s == On ? throw new TimeoutException() : Task.CompletedTask,
            ["A"],
            () => discarded = true));

        Assert.True(discarded);
    }

    [Fact]
    public async Task Purge_NoTables_OnlyTogglesChecks()
    {
        var log = new List<string>();

        await GaleraDatabaseProvider.PurgeAsync(s => { log.Add(s); return Task.CompletedTask; }, [], () => { });

        Assert.Equal([Off, On], log);
    }

    [Fact]
    public async Task Purge_Lab_PooledSessionKeepsChecksOn()
    {
        var cs = Environment.GetEnvironmentVariable(GaleraDatabaseCreatorTests.TestDbVariable);
        if (string.IsNullOrEmpty(cs))
        {
            return; // lab-only
        }

        // One pooled session, never reset between users: exactly the ConnectionReset=false case.
        cs = new MySqlConnectionStringBuilder(cs)
        {
            ApplicationName = "purge-" + Guid.NewGuid().ToString("N"),
            ConnectionReset = false,
            MaximumPoolSize = 1,
        }.ConnectionString;
        var suffix = Guid.NewGuid().ToString("N")[..8];
        string parent = "jm_purge_p_" + suffix, child = "jm_purge_c_" + suffix;
        var provider = new GaleraDatabaseProvider(null!, NullLogger<GaleraDatabaseProvider>.Instance);

        await using (var admin = new MySqlConnection(cs + ";Pooling=false"))
        {
            await admin.OpenAsync();
            await using var cmd = admin.CreateCommand();
            cmd.CommandText = $"CREATE TABLE `{parent}` (Id INT PRIMARY KEY) ENGINE=InnoDB;"
                + $"CREATE TABLE `{child}` (Id INT PRIMARY KEY, P INT NOT NULL, FOREIGN KEY (P) REFERENCES `{parent}` (Id)) ENGINE=InnoDB;"
                + $"INSERT INTO `{parent}` VALUES (1),(2); INSERT INTO `{child}` VALUES (1,1),(2,2);";
            await cmd.ExecuteNonQueryAsync();
        }

        try
        {
            // Parent first: only works with the checks off.
            using (var ctx = GaleraDatabaseCreatorTests.CreateContext(cs))
            {
                await provider.PurgeDatabase(ctx, [parent, child]);
            }

            await AssertChecksOnAndCount(cs, parent, 0);

            // A failing purge (unknown table after a real one) must still leave the session's checks on.
            await using (var admin = new MySqlConnection(cs + ";Pooling=false"))
            {
                await admin.OpenAsync();
                await using var cmd = admin.CreateCommand();
                cmd.CommandText = $"INSERT INTO `{parent}` VALUES (3)";
                await cmd.ExecuteNonQueryAsync();
            }

            using (var ctx = GaleraDatabaseCreatorTests.CreateContext(cs))
            {
                await Assert.ThrowsAsync<MySqlException>(() => provider.PurgeDatabase(ctx, [parent, "jm_purge_missing_" + suffix]));
            }

            await AssertChecksOnAndCount(cs, parent, null);
        }
        finally
        {
            MySqlConnection.ClearAllPools();
            await using var admin = new MySqlConnection(cs + ";Pooling=false");
            await admin.OpenAsync();
            await using var cmd = admin.CreateCommand();
            cmd.CommandText = $"DROP TABLE IF EXISTS `{child}`; DROP TABLE IF EXISTS `{parent}`;";
            await cmd.ExecuteNonQueryAsync();
        }
    }

    private static async Task AssertChecksOnAndCount(string cs, string parent, long? expectedRows)
    {
        // Same pool (MaximumPoolSize=1) => same server session the purge used.
        using var ctx = GaleraDatabaseCreatorTests.CreateContext(cs);
        await ctx.Database.OpenConnectionAsync();
        try
        {
            var conn = (MySqlConnection)ctx.Database.GetDbConnection();
            Assert.Equal(1, await GaleraDatabaseCreatorTests.ScalarAsync(conn, "SELECT @@SESSION.foreign_key_checks"));
            if (expectedRows is not null)
            {
                Assert.Equal(expectedRows, await GaleraDatabaseCreatorTests.ScalarAsync(conn, $"SELECT COUNT(*) FROM `{parent}`"));
            }
        }
        finally
        {
            await ctx.Database.CloseConnectionAsync();
        }
    }
}
