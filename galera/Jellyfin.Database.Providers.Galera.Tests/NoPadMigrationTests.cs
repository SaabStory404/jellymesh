using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Migrations;
using MySqlConnector;
using Xunit;

namespace Jellyfin.Database.Providers.Galera.Tests;

public class NoPadMigrationTests
{
    /// <summary>
    /// An EMPTY, disposable MySQL/Galera database. The test runs the provider's migrations up and down
    /// in it, so never point it at a database holding a library. Unset: the test returns.
    /// </summary>
    private const string EmptyDbVariable = "JELLYMESH_TEST_EMPTY_DB";

    private const string InitialCreate = "20260926223404_InitialCreate";

    [Fact]
    public async Task Migration_Lab_ConvertsPadSpaceColumnsAndAcceptsTrailingSpaceVariants()
    {
        var cs = Environment.GetEnvironmentVariable(EmptyDbVariable);
        if (string.IsNullOrEmpty(cs))
        {
            return; // lab-only
        }

        await using var context = GaleraDatabaseCreatorTests.CreateContext(cs);
        var migrator = context.GetService<IMigrator>();

        // Schema as it was deployed before the fix: every string column utf8mb4_bin (PAD SPACE).
        await migrator.MigrateAsync(InitialCreate);
        var before = await CountColumns(cs, "utf8mb4_bin");
        Assert.True(before > 0);

        // Up and Down round-trip on an empty schema.
        await migrator.MigrateAsync();
        Assert.Equal(0, await CountColumns(cs, "utf8mb4_bin"));
        Assert.Equal(before, await CountColumns(cs, GaleraModelConvention.Collation));
        await migrator.MigrateAsync(InitialCreate);
        Assert.Equal(before, await CountColumns(cs, "utf8mb4_bin"));

        // The production failure: a tag with a trailing space blocks the trimmed tag.
        await InsertItemValue(cs, "family business ");
        var duplicate = await Assert.ThrowsAsync<MySqlException>(() => InsertItemValue(cs, "family business"));
        Assert.Equal(MySqlErrorCode.DuplicateKeyEntry, duplicate.ErrorCode);

        // After the migration both are distinct values, as in SQLite.
        await migrator.MigrateAsync();
        await InsertItemValue(cs, "family business");
        Assert.Equal(0, await CountColumns(cs, "utf8mb4_bin"));
        Assert.Equal(2L, await Scalar(cs, "SELECT COUNT(*) FROM ItemValues WHERE Type = 4"));

        // A captured string list is bound through JSON_TABLE with the convention collation; it must
        // match the columns' collation or MySQL raises "Illegal mix of collations" (error 1267).
        // EF.Parameter forces the JSON_TABLE binding (EF inlines small captured arrays otherwise), as
        // Jellyfin's own list filters do.
        var wanted = new[] { "family business", "absent value" };
        await using var query = GaleraDatabaseCreatorTests.CreateContext(cs);
        Assert.Equal(1, await query.ItemValues.CountAsync(v => EF.Parameter(wanted).Contains(v.Value)));
    }

    private static async Task InsertItemValue(string cs, string value)
    {
        await using var connection = new MySqlConnection(cs);
        await connection.OpenAsync();
        await using var insert = connection.CreateCommand();
        insert.CommandText = "INSERT INTO ItemValues (ItemValueId, Type, Value, CleanValue) VALUES (@id, 4, @v, @v)";
        insert.Parameters.AddWithValue("@id", Guid.NewGuid().ToString());
        insert.Parameters.AddWithValue("@v", value);
        await insert.ExecuteNonQueryAsync();
    }

    private static async Task<long> CountColumns(string cs, string collation) =>
        await Scalar(cs, $"SELECT COUNT(*) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND COLLATION_NAME = '{collation}'");

    private static async Task<long> Scalar(string cs, string sql)
    {
        await using var connection = new MySqlConnection(cs);
        await connection.OpenAsync();
        await using var command = connection.CreateCommand();
        command.CommandText = sql;
        return Convert.ToInt64(await command.ExecuteScalarAsync(), System.Globalization.CultureInfo.InvariantCulture);
    }
}
