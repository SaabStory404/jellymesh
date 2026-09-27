// jellyfin-dbmigrate — move a Jellyfin database between SQLite and Galera (MySQL / Percona XtraDB
// Cluster), in either direction, through Jellyfin's own EF model.
//
//   jellyfin-dbmigrate model  --from sqlite:/config/data/data/jellyfin.db
//   jellyfin-dbmigrate copy   --from sqlite:/path/jellyfin.db  --to "galera:Server=...;Database=jellyfin;..."
//   jellyfin-dbmigrate copy   --from "galera:Server=..."        --to sqlite:/path/new-jellyfin.db
//
// Jellyfin must be stopped. The target schema is created by the target provider's own migrations;
// every table is copied in foreign-key order with its original keys; Jellyfin's code-migration
// history is carried over (without it Jellyfin re-runs years of data migrations on first start).
using System.Collections;
using System.Data.Common;
using System.Diagnostics;
using System.Reflection;
using Jellyfin.Database.Implementations;
using Jellyfin.Database.Implementations.DbConfiguration;
using Jellyfin.Database.Implementations.Locking;
using Jellyfin.Database.Providers.Galera;
using Jellyfin.Database.Providers.Sqlite;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.Extensions.Logging.Abstractions;

const int BatchSize = 1000;

var mode = args.FirstOrDefault();
string? Arg(string name) => args.SkipWhile(a => a != name).Skip(1).FirstOrDefault();
var from = Arg("--from");
var to = Arg("--to");

if (mode is not ("model" or "copy" or "verify") || from is null || (mode != "model" && to is null))
{
    Console.Error.WriteLine("usage: jellyfin-dbmigrate model --from <db> | copy|verify --from <db> --to <db>   (db = sqlite:<path> | galera:<connection string>)");
    return 2;
}

if (mode == "verify")
{
    using var a = Open(from);
    using var b = Open(to!);
    var bad = 0;
    foreach (var e in CopyOrder(a.Model))
    {
        var (count, diffs, sample) = Compare(a, b, e);
        bad += diffs;
        Console.WriteLine($"  {e.GetTableName(),-28} {count,8} rows  {(diffs == 0 ? "identical" : $"{diffs} DIFFERENCES e.g. {sample}")}");
    }

    Console.WriteLine(bad == 0 ? "verify: every row and column identical" : $"verify: {bad} differences");
    return bad == 0 ? 0 : 1;
}

using var source = Open(from);
if (Arg("--probe") is not null)
{
    // Which Contains() shapes does the provider translate?
    var ids = source.UserData.AsNoTracking().Select(u => u.ItemId).Take(3).ToList();
    var arr = ids.ToArray();
    void Try(string name, Func<int> q)
    {
        try
        {
            Console.WriteLine($"  {name,-34} ok ({q()} rows)");
        }
        catch (Exception ex)
        {
            Console.WriteLine($"  {name,-34} FAILS: {ex.GetType().Name}: {ex.Message.Split('\n')[0][..Math.Min(90, ex.Message.Split('\n')[0].Length)]}");
        }
    }

    Try("inline constant array", () => source.UserData.Where(u => new[] { arr[0], arr[1] }.Contains(u.ItemId)).Count());
    Try("captured List<Guid>.Contains", () => source.UserData.Where(u => ids.Contains(u.ItemId)).Count());
    Try("captured Guid[].Contains", () => source.UserData.Where(u => arr.Contains(u.ItemId)).Count());
    Try("captured Enumerable.Contains", () => source.UserData.Where(u => Enumerable.Contains(ids, u.ItemId)).Count());
    // Jellyfin 12.1 binds id lists as ONE collection parameter (JellyfinQueryHelperExtensions.WhereOneOrMany).
    Try("EF.Parameter(List<Guid>) (Jellyfin)", () => source.UserData.WhereOneOrMany(ids, u => u.ItemId).Count());
    var keys = source.UserData.AsNoTracking().Select(u => u.CustomDataKey).Take(3).ToList();
    Try("EF.Parameter(List<string>)", () => source.UserData.Where(u => EF.Parameter(keys).Contains(u.CustomDataKey)).Count());
    // /Items shape: the same id-list subquery embedded several times, incl. NOT IN and UNION.
    var sub = source.UserData.WhereOneOrMany(ids, u => u.ItemId).Select(u => u.ItemId);
    Try("reused EF.Parameter subquery", () => source.UserData
        .Where(u => sub.Contains(u.ItemId) && !source.UserData.Where(x => sub.Contains(x.ItemId) && x.UserId != u.UserId).Select(x => x.ItemId).Contains(u.ItemId))
        .Select(u => u.ItemId).Union(sub).Count());
    var q = source.UserData.Where(u => arr.Contains(u.ItemId));
    Console.WriteLine("  expression: " + q.Expression);
    try
    {
        q.Count();
    }
    catch (Exception ex)
    {
        Console.WriteLine(string.Join("\n", ex.ToString().Split('\n').Take(12)));
    }

    return 0;
}

if (mode == "model")
{
    foreach (var e in CopyOrder(source.Model))
    {
        var shadow = e.GetProperties().Where(p => p.IsShadowProperty()).Select(p => p.Name).ToArray();
        Console.WriteLine($"{e.GetTableName(),-28} {e.ClrType.Name,-28} rows={Count(source, e),8}"
            + (shadow.Length > 0 ? $"  shadow=[{string.Join(",", shadow)}]" : string.Empty)
            + (e.HasSharedClrType ? "  SHARED-CLR" : string.Empty)
            + (e.GetDerivedTypes().Any() ? $"  derived={e.GetDerivedTypes().Count()}" : string.Empty));
    }

    return 0;
}

using var target = Open(to!);
var total = Stopwatch.StartNew();
Console.WriteLine($"creating target schema ({Kind(to!)} migrations)...");
target.Database.Migrate();

var targetConn = target.Database.GetDbConnection();
target.Database.OpenConnection();
Exec(targetConn, Kind(to!) == "galera" ? "SET SESSION foreign_key_checks = 0" : "PRAGMA foreign_keys = OFF");
target.ChangeTracker.AutoDetectChangesEnabled = false;

// Migrations seed a few rows (e.g. a placeholder BaseItem 00000000-...-0001) that the source
// already has; clear them so the source's rows go in unchanged.
var tq0 = Kind(to!) == "galera" ? '`' : '"';
foreach (var entity in CopyOrder(source.Model))
{
    if (Count(target, entity) > 0)
    {
        Exec(targetConn, $"DELETE FROM {tq0}{entity.GetTableName()}{tq0}");
        Console.WriteLine($"  cleared seeded rows in {entity.GetTableName()}");
    }
}

long rows = 0;
foreach (var entity in CopyOrder(source.Model))
{
    var sw = Stopwatch.StartNew();
    var n = CopyTable(source, target, entity);
    rows += n;
    Console.WriteLine($"  {entity.GetTableName(),-28} {n,8} rows  {sw.Elapsed.TotalSeconds,6:F1}s");
}

var history = CopyCodeMigrationHistory(source, target);
Exec(targetConn, Kind(to!) == "galera" ? "SET SESSION foreign_key_checks = 1" : "PRAGMA foreign_keys = ON");
Console.WriteLine($"done: {rows} rows, {history} code-migration history rows, {total.Elapsed.TotalSeconds:F1}s");
return 0;

// ---------------------------------------------------------------------------------------------

static string Kind(string spec) => spec[..spec.IndexOf(':')];

static JellyfinDbContext Open(string spec)
{
    var kind = Kind(spec);
    var value = spec[(kind.Length + 1)..];
    var options = new DbContextOptionsBuilder<JellyfinDbContext>();
    IJellyfinDatabaseProvider provider;
    var custom = new CustomDatabaseOptions { PluginName = "dbmigrate", PluginAssembly = "dbmigrate", ConnectionString = value };
    if (kind == "sqlite")
    {
        provider = new SqliteDatabaseProvider(null!, NullLogger<SqliteDatabaseProvider>.Instance);
        custom.Options.Add(new CustomDatabaseOption { Key = "path", Value = value });
        provider.Initialise(options, new DatabaseConfigurationOptions { DatabaseType = "Jellyfin-SQLite", CustomProviderOptions = custom });
    }
    else if (kind == "galera")
    {
        provider = new GaleraDatabaseProvider(null!, NullLogger<GaleraDatabaseProvider>.Instance);
        provider.Initialise(options, new DatabaseConfigurationOptions { DatabaseType = "PLUGIN_PROVIDER", CustomProviderOptions = custom });
    }
    else
    {
        throw new ArgumentException($"unknown database kind '{kind}' (sqlite | galera)");
    }

    return new JellyfinDbContext(options.Options, NullLogger<JellyfinDbContext>.Instance, provider, new NoLockBehavior(NullLogger<NoLockBehavior>.Instance));
}

// Root entity types that own a table, principals before dependents (self-references ignored).
static List<IEntityType> CopyOrder(IModel model)
{
    var types = model.GetEntityTypes().Where(e => e.BaseType is null && !e.IsOwned() && e.GetTableName() is not null).ToList();
    var ordered = new List<IEntityType>();
    var visiting = new HashSet<IEntityType>();
    void Visit(IEntityType e)
    {
        if (ordered.Contains(e) || !visiting.Add(e))
        {
            return;
        }

        foreach (var fk in e.GetForeignKeys())
        {
            var principal = fk.PrincipalEntityType.GetRootType();
            if (principal != e && types.Contains(principal))
            {
                Visit(principal);
            }
        }

        ordered.Add(e);
    }

    foreach (var t in types.OrderBy(t => t.GetTableName()))
    {
        Visit(t);
    }

    return ordered;
}

static IQueryable Query(DbContext ctx, IEntityType e, bool tracking)
{
    var set = e.HasSharedClrType
        ? typeof(DbContext).GetMethod(nameof(DbContext.Set), [typeof(string)])!.MakeGenericMethod(e.ClrType).Invoke(ctx, [e.Name])!
        : typeof(DbContext).GetMethod(nameof(DbContext.Set), Type.EmptyTypes)!.MakeGenericMethod(e.ClrType).Invoke(ctx, null)!;
    var method = tracking ? nameof(EntityFrameworkQueryableExtensions.AsTracking) : nameof(EntityFrameworkQueryableExtensions.AsNoTracking);
    return (IQueryable)typeof(EntityFrameworkQueryableExtensions)
        .GetMethods().First(m => m.Name == method && m.GetParameters().Length == 1)
        .MakeGenericMethod(e.ClrType).Invoke(null, [set])!;
}

static long Count(DbContext ctx, IEntityType e) => Query(ctx, e, false).Cast<object>().LongCount();

static long CopyTable(JellyfinDbContext source, JellyfinDbContext target, IEntityType e)
{
    // Shadow properties only live in the change tracker, so read those tables with tracking.
    var shadow = e.GetProperties().Where(p => p.IsShadowProperty()).ToList();
    var tracking = shadow.Count > 0;
    long n = 0;
    var batch = 0;
    foreach (var entity in (IEnumerable)Query(source, e, tracking))
    {
        var shadowValues = tracking ? shadow.Select(p => source.Entry(entity).Property(p.Name).CurrentValue).ToArray() : null;
        var entry = target.Entry(entity);
        entry.State = EntityState.Added;
        if (shadowValues is not null)
        {
            for (var i = 0; i < shadow.Count; i++)
            {
                entry.Property(shadow[i].Name).CurrentValue = shadowValues[i];
            }
        }

        n++;
        if (++batch == BatchSize)
        {
            target.SaveChanges();
            target.ChangeTracker.Clear();
            if (tracking)
            {
                source.ChangeTracker.Clear();
            }

            batch = 0;
        }
    }

    target.SaveChanges();
    target.ChangeTracker.Clear();
    source.ChangeTracker.Clear();
    return n;
}

// Jellyfin records its code (data) migrations in __EFMigrationsHistory next to the provider's
// schema migrations. Schema migrations are provider-specific and carry EF's 3-part version; code
// migrations carry Jellyfin's 4-part version and must follow the data.
static int CopyCodeMigrationHistory(JellyfinDbContext source, JellyfinDbContext target)
{
    var q = source.Database.ProviderName!.Contains("MySQL", StringComparison.OrdinalIgnoreCase) ? '`' : '"';
    var rows = new List<(string Id, string Version)>();
    var sconn = source.Database.GetDbConnection();
    source.Database.OpenConnection();
    using (var cmd = sconn.CreateCommand())
    {
        cmd.CommandText = $"SELECT {q}MigrationId{q}, {q}ProductVersion{q} FROM {q}__EFMigrationsHistory{q}";
        using var r = cmd.ExecuteReader();
        while (r.Read())
        {
            rows.Add((r.GetString(0), r.GetString(1)));
        }
    }

    var tq = target.Database.ProviderName!.Contains("MySQL", StringComparison.OrdinalIgnoreCase) ? '`' : '"';
    var tconn = target.Database.GetDbConnection();
    var existing = new HashSet<string>();
    using (var cmd = tconn.CreateCommand())
    {
        cmd.CommandText = $"SELECT {tq}MigrationId{tq} FROM {tq}__EFMigrationsHistory{tq}";
        using var r = cmd.ExecuteReader();
        while (r.Read())
        {
            existing.Add(r.GetString(0));
        }
    }

    var code = rows.Where(r => r.Version.Split('.').Length == 4 && !existing.Contains(r.Id)).ToList();
    foreach (var (id, version) in code)
    {
        using var cmd = tconn.CreateCommand();
        cmd.CommandText = $"INSERT INTO {tq}__EFMigrationsHistory{tq} ({tq}MigrationId{tq}, {tq}ProductVersion{tq}) VALUES (@id, @v)";
        var p1 = cmd.CreateParameter();
        p1.ParameterName = "@id";
        p1.Value = id;
        var p2 = cmd.CreateParameter();
        p2.ParameterName = "@v";
        p2.Value = version;
        cmd.Parameters.Add(p1);
        cmd.Parameters.Add(p2);
        cmd.ExecuteNonQuery();
    }

    return code.Count;
}

// Every mapped property of every row, matched by primary key. DateTimes are compared to the tick:
// the Galera provider stores ticks, since Jellyfin derives image tags and chapter image file names
// from them.
static (long Count, int Diffs, string? Sample) Compare(JellyfinDbContext a, JellyfinDbContext b, IEntityType e)
{
    var key = e.FindPrimaryKey()!.Properties;
    var props = e.GetProperties().Where(p => !p.IsShadowProperty()).ToList();
    Dictionary<string, object?[]> Load(JellyfinDbContext ctx)
    {
        var map = new Dictionary<string, object?[]>();
        foreach (var row in (IEnumerable)Query(ctx, e, false))
        {
            var k = string.Join("|", key.Select(p => Norm(p.PropertyInfo!.GetValue(row))));
            map[k] = props.Select(p => p.PropertyInfo?.GetValue(row) ?? p.FieldInfo?.GetValue(row)).ToArray();
        }

        return map;
    }

    var left = Load(a);
    var right = Load(b);
    var diffs = 0;
    string? sample = null;
    foreach (var (k, lv) in left)
    {
        if (!right.TryGetValue(k, out var rv))
        {
            diffs++;
            sample ??= $"missing key {k}";
            continue;
        }

        for (var i = 0; i < props.Count; i++)
        {
            if (Norm(lv[i]) != Norm(rv[i]))
            {
                diffs++;
                sample ??= $"{k}.{props[i].Name}: '{Norm(lv[i])}' vs '{Norm(rv[i])}'";
            }
        }
    }

    diffs += right.Keys.Count(k => !left.ContainsKey(k));
    return (left.Count, diffs, sample);

    static string Norm(object? v) => v switch
    {
        null => "<null>",
        DateTime d => d.Ticks + "t",
        Guid g => g.ToString("D"),
        float f => f.ToString("R", System.Globalization.CultureInfo.InvariantCulture),
        double d => d.ToString("R", System.Globalization.CultureInfo.InvariantCulture),
        byte[] bytes => Convert.ToHexString(bytes),
        IEnumerable items when v is not string => "[" + string.Join(",", items.Cast<object?>().Select(Norm)) + "]",
        _ => Convert.ToString(v, System.Globalization.CultureInfo.InvariantCulture) ?? "<null>",
    };
}

static void Exec(DbConnection conn, string sql)
{
    using var cmd = conn.CreateCommand();
    cmd.CommandText = sql;
    cmd.ExecuteNonQuery();
}
