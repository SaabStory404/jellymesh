using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Metadata;
using MySqlConnector;
using Xunit;

namespace Jellyfin.Database.Providers.Galera.Tests;

public class CollationTests
{
    // Building the model needs no server: the provider pins the server version (8.4.0).
    private const string OfflineConnectionString = "Server=127.0.0.1;Port=1;Database=jellyfin;User ID=x;Password=x";

    [Fact]
    public void Model_MatchesMigrationSnapshot()
    {
        // EF refuses to migrate when the model and the last migration's snapshot differ
        // (PendingModelChangesWarning), which would stop Jellyfin at startup.
        using var context = GaleraDatabaseCreatorTests.CreateContext(OfflineConnectionString);
        Assert.False(context.Database.HasPendingModelChanges());
    }

    [Fact]
    public void Model_EveryStringColumnUsesTheConventionCollation()
    {
        using var context = GaleraDatabaseCreatorTests.CreateContext(OfflineConnectionString);
        // The runtime model drops collations; the design-time model keeps them.
        var wrong = context.GetService<IDesignTimeModel>().Model.GetEntityTypes()
            .SelectMany(e => e.GetProperties())
            .Where(p => p.ClrType == typeof(string) && p.GetCollation() != GaleraModelConvention.Collation)
            .Select(p => $"{p.DeclaringType.Name}.{p.Name}={p.GetCollation()}")
            .ToList();
        Assert.Empty(wrong);
    }

    [Fact]
    public async Task Collation_Lab_IsNoPadAndBinary()
    {
        var cs = Environment.GetEnvironmentVariable(GaleraDatabaseCreatorTests.TestDbVariable);
        if (string.IsNullOrEmpty(cs))
        {
            return; // lab-only
        }

        await using var connection = new MySqlConnection(cs);
        await connection.OpenAsync();

        await using (var pad = connection.CreateCommand())
        {
            pad.CommandText = "SELECT PAD_ATTRIBUTE FROM information_schema.COLLATIONS WHERE COLLATION_NAME = @c";
            pad.Parameters.AddWithValue("@c", GaleraModelConvention.Collation);
            Assert.Equal("NO PAD", await pad.ExecuteScalarAsync());
        }

        // The ItemValues shape: a unique (Type, Value) index must keep SQLite's BINARY distinctions.
        await using (var setup = connection.CreateCommand())
        {
            setup.CommandText =
                "CREATE TEMPORARY TABLE jm_collation_probe (Type int NOT NULL, Value varchar(700) CHARACTER SET utf8mb4 " +
                $"COLLATE {GaleraModelConvention.Collation} NOT NULL, UNIQUE KEY ux (Type, Value)) ENGINE=InnoDB";
            await setup.ExecuteNonQueryAsync();
        }

        foreach (var value in new[] { "family business", "family business ", "Family Business" })
        {
            await using var insert = connection.CreateCommand();
            insert.CommandText = "INSERT INTO jm_collation_probe (Type, Value) VALUES (4, @v)";
            insert.Parameters.AddWithValue("@v", value);
            await insert.ExecuteNonQueryAsync(); // utf8mb4_bin (PAD SPACE) threw Duplicate entry on the second
        }

        await using var count = connection.CreateCommand();
        count.CommandText = "SELECT COUNT(*) FROM jm_collation_probe WHERE Value = 'family business'";
        Assert.Equal(1L, Convert.ToInt64(await count.ExecuteScalarAsync(), System.Globalization.CultureInfo.InvariantCulture));
    }
}
