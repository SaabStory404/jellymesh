using Jellyfin.Database.Implementations;
using Jellyfin.Database.Implementations.DbConfiguration;
using Jellyfin.Database.Implementations.Locking;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Design;
using Microsoft.Extensions.Logging.Abstractions;

namespace Jellyfin.Database.Providers.Galera.Migrations;

/// <summary>
/// Design-time factory used only by `dotnet ef migrations add`; mirrors Jellyfin's
/// SqliteDesignTimeJellyfinDbFactory. No server is contacted while generating migrations.
/// </summary>
internal sealed class GaleraDesignTimeJellyfinDbFactory : IDesignTimeDbContextFactory<JellyfinDbContext>
{
    public JellyfinDbContext CreateDbContext(string[] args)
    {
        var provider = new GaleraDatabaseProvider(null!, NullLogger<GaleraDatabaseProvider>.Instance);
        var optionsBuilder = new DbContextOptionsBuilder<JellyfinDbContext>();
        provider.Initialise(optionsBuilder, new DatabaseConfigurationOptions
        {
            DatabaseType = "PLUGIN_PROVIDER",
            CustomProviderOptions = new CustomDatabaseOptions
            {
                PluginName = "JellyMesh Galera",
                PluginAssembly = "Jellyfin.Database.Providers.Galera",
                // JM_MYSQL_CONN lets `dotnet ef database update` target a lab server.
                ConnectionString = Environment.GetEnvironmentVariable("JM_MYSQL_CONN")
                    ?? "Server=localhost;Database=jellyfin;Uid=jellyfin;Pwd=design-time",
            },
        });

        return new JellyfinDbContext(
            optionsBuilder.Options,
            NullLogger<JellyfinDbContext>.Instance,
            provider,
            new NoLockBehavior(NullLogger<NoLockBehavior>.Instance));
    }
}
