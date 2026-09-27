using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Metadata.Builders;
using Microsoft.EntityFrameworkCore.Metadata.Conventions;
using Microsoft.EntityFrameworkCore.Storage.ValueConversion;

namespace Jellyfin.Database.Providers.Galera;

/// <summary>
/// Model rules that make Jellyfin's EF model valid and loss-free on MySQL 8 / Galera. Runs as a
/// model-finalizing convention: JellyfinDbContext calls the provider's OnModelCreating *before*
/// applying its own entity configuration (JellyfinDbContext.cs:328 vs :332), so the rules must run
/// once the whole model exists.
/// </summary>
/// <remarks>
/// Jellyfin declares most strings unbounded. MySQL can only index bounded columns (or prefixes of
/// long ones) and caps every index at 3072 bytes (768 utf8mb4 characters). Oracle's provider
/// silently turns indexed strings into varchar(255), which truncates real data (MEASURED: a real
/// library has 264-character paths). So:
/// <list type="bullet">
/// <item>Strings in a primary key or unique index get a bounded varchar sized to fit the key limit
/// (primary keys and unique indexes cannot use prefixes without changing their meaning).</item>
/// <item>Every other indexed string stays unbounded (longtext); its index gets a prefix length
/// (the provider's IndexPrefixLength), the text columns of one index sharing what is left of the
/// 3072 bytes after its fixed-width columns. No value is ever truncated.</item>
/// <item>Binary collation on every string column, like SQLite's default, so case-only differences
/// stay distinct.</item>
/// <item>DateTime stored as BIGINT ticks, read back as UTC (as Jellyfin's SQLite provider does).
/// MySQL DATETIME(6) keeps microseconds, but Jellyfin derives identities from full ticks: image
/// cache tags hash DateModified.Ticks and chapter image file names embed it (PathManager), so
/// truncation changed every image tag after a migration (MEASURED) and would orphan chapter images.
/// No Jellyfin query applies DateTime members (.Date, AddDays, DateTime.UtcNow) inside SQL, so the
/// columns only need ordering and comparison, which ticks keep.</item>
/// </list>
/// </remarks>
internal sealed class GaleraModelConvention : IModelFinalizingConvention
{
    /// <summary>Collation for every string column.</summary>
    public const string Collation = "utf8mb4_bin";

    /// <summary>Default length for strings in primary keys and unique indexes.</summary>
    public const int KeyStringLength = 512;

    private const int IndexByteLimit = 3072;
    private const int BytesPerChar = 4; // utf8mb4
    private const int MaxPrefixChars = 255;

    // Per-column overrides where a composite key has room for more.
    private static readonly Dictionary<(string Table, string Column), int> KeyLengthOverrides = new()
    {
        [("ItemValues", "Value")] = 700, // unique (Type int, Value): 4 + 2800 bytes
    };

    private static readonly ValueConverter<DateTime, long> TicksConverter =
        new(v => v.Ticks, v => new DateTime(v, DateTimeKind.Utc));

    private static readonly ValueConverter<DateTime?, long?> NullableTicksConverter =
        new(v => v.HasValue ? v.Value.Ticks : null, v => v.HasValue ? new DateTime(v.Value, DateTimeKind.Utc) : null);

    public void ProcessModelFinalizing(IConventionModelBuilder modelBuilder, IConventionContext<IConventionModelBuilder> context)
    {
        foreach (var entity in modelBuilder.Metadata.GetEntityTypes())
        {
            var table = entity.GetTableName();
            if (table is null)
            {
                continue;
            }

            var keyProps = entity.GetKeys().SelectMany(k => k.Properties)
                .Concat(entity.GetIndexes().Where(i => i.IsUnique).SelectMany(i => i.Properties))
                .ToHashSet();
            var indexedProps = entity.GetIndexes().SelectMany(i => i.Properties).ToHashSet();

            foreach (var prop in entity.GetProperties())
            {
                if (prop.ClrType == typeof(string))
                {
                    prop.Builder.UseCollation(Collation);
                    if (prop.GetMaxLength() is null && prop.GetColumnType() is null)
                    {
                        if (keyProps.Contains(prop))
                        {
                            prop.Builder.HasMaxLength(KeyLengthOverrides.GetValueOrDefault((table, prop.GetColumnName()), KeyStringLength));
                        }
                        else if (indexedProps.Contains(prop))
                        {
                            prop.Builder.HasColumnType("longtext");
                        }
                    }
                }
                else if (PrimitiveCollectionElement(prop.ClrType) is { } element)
                {
                    // Oracle's provider has no primitive-collection support: it wrote
                    // List<long>.ToString() ("System.Collections.Generic.List`1[...]") into
                    // KeyframeData.KeyframeTicks (MEASURED). Store JSON arrays, as SQLite does.
                    var converter = (ValueConverter)typeof(GaleraModelConvention)
                        .GetMethod(nameof(JsonCollectionConverter), System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Static)!
                        .MakeGenericMethod(prop.ClrType, element).Invoke(null, null)!;
                    prop.Builder.HasConversion(converter);
                    prop.Builder.HasColumnType("longtext");
                }
                else if (prop.ClrType == typeof(float) || prop.ClrType == typeof(float?))
                {
                    // MySQL FLOAT comes back over the text protocol rounded to 6 significant digits
                    // (MEASURED: AverageFrameRate 23.976025 -> 23.976). DOUBLE holds every float exactly.
                    prop.Builder.HasColumnType("double");
                }
                else if (prop.ClrType == typeof(DateTime))
                {
                    prop.Builder.HasConversion(TicksConverter);
                }
                else if (prop.ClrType == typeof(DateTime?))
                {
                    prop.Builder.HasConversion(NullableTicksConverter);
                }
            }

            foreach (var index in entity.GetIndexes())
            {
                var text = index.Properties.Select(p => p.GetColumnType() == "longtext").ToArray();
                if (!text.Any(t => t))
                {
                    continue;
                }

                var fixedBytes = index.Properties.Where(p => p.GetColumnType() != "longtext").Sum(FixedBytes);
                var prefix = Math.Min(MaxPrefixChars, (IndexByteLimit - fixedBytes) / BytesPerChar / text.Count(t => t));
                index.SetPrefixLength(text.Select(t => t ? prefix : 0).ToArray(), fromDataAnnotation: false);
            }
        }
    }

    private static Type? PrimitiveCollectionElement(Type t)
    {
        if (t == typeof(string) || t == typeof(byte[]))
        {
            return null;
        }

        var element = t.IsArray ? t.GetElementType()
            : t.IsGenericType && typeof(System.Collections.IEnumerable).IsAssignableFrom(t) ? t.GetGenericArguments().FirstOrDefault() : null;
        return element is not null && (element.IsPrimitive || element == typeof(string) || element == typeof(Guid) || element == typeof(decimal))
            ? element
            : null;
    }

    private static ValueConverter JsonCollectionConverter<TCollection, TElement>()
        where TCollection : class, IEnumerable<TElement>
        => new ValueConverter<TCollection?, string?>(
            v => v == null ? null : System.Text.Json.JsonSerializer.Serialize(v.ToList(), (System.Text.Json.JsonSerializerOptions?)null),
            s => s == null ? null : (TCollection)(object)System.Text.Json.JsonSerializer.Deserialize<List<TElement>>(s, (System.Text.Json.JsonSerializerOptions?)null)!);

    private static int FixedBytes(IConventionProperty p)
    {
        var t = Nullable.GetUnderlyingType(p.ClrType) ?? p.ClrType;
        return t switch
        {
            _ when t == typeof(string) => (p.GetMaxLength() ?? 255) * BytesPerChar,
            _ when t == typeof(Guid) => 36 * BytesPerChar,
            _ when t == typeof(long) || t == typeof(double) || t == typeof(DateTime) => 8,
            _ when t == typeof(bool) || t == typeof(byte) => 1,
            _ => 4,
        };
    }
}
