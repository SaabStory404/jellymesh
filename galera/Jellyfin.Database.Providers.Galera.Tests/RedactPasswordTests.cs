using Xunit;

namespace Jellyfin.Database.Providers.Galera.Tests;

public class RedactPasswordTests
{
    private const string Secret = "s3cr3t-TAIL-value";

    [Theory]
    [InlineData("Server=db;User ID=jf;Password=" + Secret + ";Database=jellyfin")]
    [InlineData("Server=db;User ID=jf;Password=\"pre;" + Secret + "\";Database=jellyfin")]
    [InlineData("Server=db;User ID=jf;Password='pre;" + Secret + "';Database=jellyfin")]
    [InlineData("Server=db;User ID=jf;Pwd=\"pre;" + Secret + "\";Database=jellyfin")]
    [InlineData("Server=db;Password=\"a\"\"b;" + Secret + "\";Database=jellyfin")]
    public void PasswordNeverAppears(string connectionString)
    {
        var redacted = GaleraDatabaseProvider.RedactPassword(connectionString);

        Assert.DoesNotContain(Secret, redacted, StringComparison.OrdinalIgnoreCase);
        Assert.DoesNotContain("pre;", redacted, StringComparison.Ordinal);
        Assert.Contains("*****", redacted, StringComparison.Ordinal);
        Assert.Contains("jellyfin", redacted, StringComparison.OrdinalIgnoreCase);
    }

    [Theory]
    // The builder rejects the unknown key, so these go through the regex fallback, which must still
    // treat a quoted value as one token.
    [InlineData("Bogus Key=1;Password=\"pre;" + Secret + "\";Database=jellyfin")]
    [InlineData("Bogus Key=1;pwd='pre;" + Secret + "';Database=jellyfin")]
    [InlineData("Bogus Key=1;Password=\"a\"\"b;" + Secret + "\";Database=jellyfin")]
    [InlineData("Bogus Key=1;Password=" + Secret + ";Database=jellyfin")]
    [InlineData("Port=abc;Password=\"pre;" + Secret + "\";Database=jellyfin")]
    public void FallbackMasksQuotedValues(string connectionString)
    {
        var redacted = GaleraDatabaseProvider.RedactPassword(connectionString);

        Assert.DoesNotContain(Secret, redacted, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("*****", redacted, StringComparison.Ordinal);
        Assert.EndsWith(";Database=jellyfin", redacted, StringComparison.Ordinal);
    }

    [Fact]
    public void NoPasswordIsLeftAlone()
    {
        var redacted = GaleraDatabaseProvider.RedactPassword("Server=db;User ID=jf;Database=jellyfin");

        Assert.DoesNotContain("*****", redacted, StringComparison.Ordinal);
        Assert.Contains("jellyfin", redacted, StringComparison.OrdinalIgnoreCase);
    }
}
