using System.Data.Common;
using Microsoft.EntityFrameworkCore.Storage;
using Pomelo.EntityFrameworkCore.MySql.Storage.Internal;

namespace Jellyfin.Database.Providers.Galera;

/// <summary>
/// Pomelo's CanConnect goes through Exists(), which opens a "master" connection with Pooling=false:
/// a fresh TCP + TLS + auth handshake on every call. Jellyfin's /health (DbContextFactoryHealthCheck)
/// calls CanConnectAsync on every probe, so each probe paid a full new connection (MEASURED, lab:
/// 1.00 new connection per /health).
/// CanConnect here runs <c>SELECT 1</c> on the context's own pooled connection instead.
/// Exists(), Create() and migrations are unchanged.
/// </summary>
/// <remarks>
/// Derives from Pomelo's "internal" creator; the plugin ships its own pinned Pomelo build, so the
/// base class cannot move under it.
/// </remarks>
#pragma warning disable EF1001 // Pomelo's creator is an "internal" API; pinned Pomelo build.
public sealed class GaleraDatabaseCreator : MySqlDatabaseCreator
{
    private readonly IMySqlRelationalConnection _connection;

    /// <summary>Initializes a new instance of the <see cref="GaleraDatabaseCreator"/> class.</summary>
    /// <param name="dependencies">EF dependencies.</param>
    /// <param name="relationalConnection">The context's (pooled) connection.</param>
    /// <param name="rawSqlCommandBuilder">Raw SQL builder.</param>
    public GaleraDatabaseCreator(
        RelationalDatabaseCreatorDependencies dependencies,
        IMySqlRelationalConnection relationalConnection,
        IRawSqlCommandBuilder rawSqlCommandBuilder)
        : base(dependencies, relationalConnection, rawSqlCommandBuilder)
    {
        _connection = relationalConnection;
    }

    /// <inheritdoc/>
    public override bool CanConnect()
    {
        try
        {
            _connection.Open(errorsExpected: true);
            try
            {
                // A real round trip: checking a connection out of the pool need not touch the server
                // (ConnectionReset=false), so opening alone proves nothing.
                using var cmd = _connection.DbConnection.CreateCommand();
                cmd.CommandText = "SELECT 1";
                cmd.ExecuteScalar();
            }
            finally
            {
                _connection.Close();
            }

            return true;
        }
        catch (DbException)
        {
            return false;
        }
    }

    /// <inheritdoc/>
    public override async Task<bool> CanConnectAsync(CancellationToken cancellationToken = default)
    {
        try
        {
            await _connection.OpenAsync(cancellationToken, errorsExpected: true).ConfigureAwait(false);
            try
            {
                var cmd = _connection.DbConnection.CreateCommand();
                await using (cmd.ConfigureAwait(false))
                {
                    cmd.CommandText = "SELECT 1";
                    await cmd.ExecuteScalarAsync(cancellationToken).ConfigureAwait(false);
                }
            }
            finally
            {
                await _connection.CloseAsync().ConfigureAwait(false);
            }

            return true;
        }
        catch (DbException)
        {
            return false;
        }
    }
}
#pragma warning restore EF1001
