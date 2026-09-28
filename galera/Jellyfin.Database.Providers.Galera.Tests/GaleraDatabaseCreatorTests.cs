using System.Globalization;
using Jellyfin.Database.Implementations;
using Jellyfin.Database.Implementations.DbConfiguration;
using Jellyfin.Database.Implementations.Locking;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Storage;
using Microsoft.Extensions.Logging.Abstractions;
using MySqlConnector;
using Xunit;

namespace Jellyfin.Database.Providers.Galera.Tests;

public class GaleraDatabaseCreatorTests
{
    /// <summary>
    /// Lab-only integration tests read a connection string from this variable (a Galera/MySQL database the
    /// user may create and drop scratch tables in). Unset: those tests return without asserting anything.
    /// </summary>
    internal const string TestDbVariable = "JELLYMESH_TEST_DB";

    internal static JellyfinDbContext CreateContext(string connectionString)
    {
        var provider = new GaleraDatabaseProvider(null!, NullLogger<GaleraDatabaseProvider>.Instance);
        var builder = new DbContextOptionsBuilder<JellyfinDbContext>();
        provider.Initialise(builder, new DatabaseConfigurationOptions
        {
            DatabaseType = "PLUGIN_PROVIDER",
            CustomProviderOptions = new CustomDatabaseOptions
            {
                PluginName = "JellyMesh Galera",
                PluginAssembly = "Jellyfin.Database.Providers.Galera.dll",
                ConnectionString = connectionString,
            },
        });
        return new JellyfinDbContext(
            builder.Options,
            NullLogger<JellyfinDbContext>.Instance,
            provider,
            new NoLockBehavior(NullLogger<NoLockBehavior>.Instance));
    }

    internal static async Task<long> ScalarAsync(MySqlConnection conn, string sql)
    {
        await using var cmd = conn.CreateCommand();
        cmd.CommandText = sql;
        return Convert.ToInt64(await cmd.ExecuteScalarAsync(), CultureInfo.InvariantCulture);
    }

    [Fact]
    public void Initialise_ReplacesDatabaseCreator()
    {
        using var ctx = CreateContext("Server=127.0.0.1;Port=1;Database=jellyfin;Uid=nobody");

        Assert.IsType<GaleraDatabaseCreator>(ctx.GetService<IRelationalDatabaseCreator>());
        Assert.IsType<GaleraDatabaseCreator>(ctx.GetService<IDatabaseCreator>());
    }

    [Fact]
    public async Task CanConnect_NoServer_ReturnsFalse()
    {
        // Nothing listens on port 1: the check must answer false, not throw.
        using var ctx = CreateContext("Server=127.0.0.1;Port=1;Database=jellyfin;Uid=nobody;ConnectionTimeout=2");

        Assert.False(ctx.Database.CanConnect());
        Assert.False(await ctx.Database.CanConnectAsync());
    }

    [Fact]
    public async Task CanConnect_Cancelled_Throws()
    {
        using var ctx = CreateContext("Server=127.0.0.1;Port=1;Database=jellyfin;Uid=nobody;ConnectionTimeout=2");
        using var cts = new CancellationTokenSource();
        await cts.CancelAsync();

        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => ctx.Database.CanConnectAsync(cts.Token));
    }

    [Fact]
    public async Task CanConnect_Lab_ReusesPooledConnection()
    {
        var cs = Environment.GetEnvironmentVariable(TestDbVariable);
        if (string.IsNullOrEmpty(cs))
        {
            return; // lab-only
        }

        // A private pool (unique Application Name) so other tests' connections do not interfere.
        cs = new MySqlConnectionStringBuilder(cs) { ApplicationName = "canconnect-" + Guid.NewGuid().ToString("N") }.ConnectionString;
        using (var warm = CreateContext(cs))
        {
            Assert.True(await warm.Database.CanConnectAsync());
        }

        await using var monitor = new MySqlConnection(cs + ";Pooling=false");
        await monitor.OpenAsync();
        const string Connections = "SELECT VARIABLE_VALUE FROM performance_schema.global_status WHERE VARIABLE_NAME = 'Connections'";
        var before = await ScalarAsync(monitor, Connections);
        for (var i = 0; i < 20; i++)
        {
            using var ctx = CreateContext(cs);
            Assert.True(i % 2 == 0 ? await ctx.Database.CanConnectAsync() : ctx.Database.CanConnect());
        }

        // Pomelo's own CanConnect opened a new unpooled connection per call (>= 20 here); slack for other
        // lab clients connecting meanwhile.
        Assert.InRange(await ScalarAsync(monitor, Connections) - before, 0, 5);
    }
}
