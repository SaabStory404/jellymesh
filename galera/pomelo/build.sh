#!/usr/bin/env bash
# Build the Pomelo MySQL EF Core provider the Galera provider ships with:
#   upstream PR #2047 (EF Core 10 support, unmerged; pinned below) + jellymesh-pomelo.patch.
# Output: $POMELO_OUT (default ~/.cache/jellymesh-vendor/pomelo) — the Galera csproj's PomeloBin.
#
# jellymesh-pomelo.patch (each MEASURED against Jellyfin 12.1 on the real library):
#   - MySqlTypeMappingPostprocessor: type-maps JSON_TABLE() over a collection parameter.
#     Jellyfin binds id lists as EF.Parameter(list); without it /UserViews fails with
#     "Expression '@p' in the SQL tree does not have a type mapping assigned".
#   - MySqlJsonTableExpression.WithAlias/Clone: the base versions returned a plain table function,
#     dropping the path + COLUMNS clause after alias renaming -> "JSON_TABLE(@p2) AS p0" syntax
#     error on /Items.
#   - Float/double literals always carry an exponent (100E0): "100" is an integer literal, so
#     search scoring's CASE .. THEN 100 came back BIGINT and GetFloat() threw.
#   - IN (subquery) is emitted as IN (SELECT * FROM (subquery) AS jm_inN) when not correlated:
#     MySQL re-ran UNION/GROUP BY IN-subqueries once per outer row (movie grid COUNT 286 ms ->
#     5.9 ms server time).
#   - JSON_TABLE() COLUMNS declare charset/collation (Guid: ascii like the char(36) keys; strings:
#     the AppContext "…JsonTableStringCollation" value, which the Galera provider sets to
#     utf8mb4_bin). Undeclared they are latin1, so id lists could not use primary keys:
#     /UserViews server time 63 -> 8 ms, batched person load 274 ms -> PK lookups.
set -euo pipefail
REPO=https://github.com/sufficit/Pomelo.EntityFrameworkCore.MySql.git
SHA=14a6e2897e6f7d16272c687e3ff17265eae38fe4   # branch upgrade/10.0.0 (PR #2047)
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=${POMELO_SRC:-$HOME/.cache/jellymesh-vendor/pomelo-src}
OUT=${POMELO_OUT:-$HOME/.cache/jellymesh-vendor/pomelo}

if [ ! -d "$SRC/.git" ]; then
  git clone --filter=blob:none "$REPO" "$SRC"
fi
git -C "$SRC" fetch -q origin "$SHA"
git -C "$SRC" checkout -q -f "$SHA"
git -C "$SRC" apply "$HERE/jellymesh-pomelo.patch"
# NuGetAudit off: a transitive build-time package (Microsoft.Build.Tasks.Git) has an advisory
# that the repo treats as an error; it does not ship in the output.
dotnet build "$SRC/src/EFCore.MySql/EFCore.MySql.csproj" -c Release -p:NuGetAudit=false -o "$OUT"
echo "$SHA + jellymesh-pomelo.patch" > "$OUT/SOURCE"
