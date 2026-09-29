# toasty-driver-mssql

A **proof-of-concept** [Toasty](https://github.com/tokio-rs/toasty) driver for
Microsoft SQL Server, speaking the TDS protocol directly through Microsoft's
[`mssql-tds`](https://github.com/microsoft/mssql-rs) client. No ODBC, no
`sqlx`, no background thread bridging to a blocking API.

> **Not for production.** See [Status](#status).

## Usage

Toasty's `Db::builder().connect(url)` dispatches on a hard-coded set of URL
schemes, and none of them is `mssql`. Build the driver from a `mssql://` URL and
hand it to `build()` instead:

```rust,ignore
use toasty_driver_mssql::Mssql;

#[derive(Debug, toasty::Model)]
struct User {
    #[key]
    #[auto]
    id: i64,
    name: String,
}

let driver = Mssql::from_url(
    "mssql://sa:Password1!@localhost:1433/mydb?encrypt=on&trust_certificate=true",
)?;

let db = toasty::Db::builder()
    .models(toasty::models!(User))
    .build(driver)
    .await?;

db.push_schema().await?;
```

```toml
[dependencies]
toasty = { version = "0.11", default-features = false }
toasty-driver-mssql = { git = "https://github.com/strawberyy-coconut/toasty-driver-mssql.git" }
```

### Connecting

`Mssql::from_url` — or `str::parse` — takes a `mssql://` URL:

```text
mssql://user:password@host:port/database?parameters
```

| Piece | Example | Notes |
|---|---|---|
| host, port | `localhost:1433` | The port defaults to 1433 |
| credentials | `sa:Password1!` | Percent-decoded, so `p@ss` is written `p%40ss` |
| database | `/mydb` | Also settable with `?database=` |
| parameters | `?encrypt=on` | See below |

| Parameter | Meaning |
|---|---|
| `database` | Database name, overriding the URL path |
| `encrypt` | `on`, `required`, `strict` or `off` (also `true`/`false`) |
| `trust_certificate` | `true` skips server certificate validation |
| `server_certificate` | Path to a DER or PEM certificate to pin |
| `host_name_in_cert` | CN or SAN expected in the server certificate |
| `application_name` | Application name reported to the server |
| `connect_timeout` | Connection timeout in seconds |

Unknown parameters are ignored, as the reference `sqlx` driver did, so a URL
written for another client still parses.

The default encryption mode is `Strict`, which is TDS 8.0 — SQL Server 2022 does
not speak it, so a connection to one wants `?encrypt=on`.

`MssqlConnectOptions` is the same configuration as a value: `parse` reads a URL
into it, `with_database` points it somewhere else, and `Mssql::from_options`
builds the driver from it.

`Mssql::new` still takes `mssql-tds`'s own `ClientContext`, and the driver
re-exports it along with the encryption types inside it, so everything that
client offers is reachable without a URL at all. Its data source is
`tcp:host,port`, the form `mssql-tds` expects.

## Examples

Four runnable examples, each against its own database — they call `reset_db`,
which drops and recreates it, so they are safe to repeat.

```bash
docker compose -f compose.dev.yaml up -d db

cargo run --example crud                        # models, schema, CRUD, transactions
cargo run --example migrations                  # schema diff → SQL, applied via the CLI
cargo run --example functions --features funcs  # the four function categories
cargo run --example spatial --features spatial  # geometry and geography columns
```

They read the connection from `DATABASE_URL`, which `.dev.env` supplies to the
development container. From the host, point the URL at the published port
instead:

```bash
DATABASE_URL='mssql://sa:Password1!@localhost:1434/testdb?encrypt=on&trust_certificate=true' \
    cargo run --example crud
```

The query parameters matter: the client's default mode is `Strict`, which TDS 8.0
is and SQL Server 2022 is not, and the development container's certificate is
self-signed.

## Migrations and the CLI

`push_schema`, which the other examples use, creates every table and index from
the models each time it runs — right for a prototype or a test, wrong for a
database that already holds data. Migrations are the other way: Toasty diffs the
models against the last snapshot, the driver renders that diff to T-SQL, and the
result is written to a file, reviewed, and applied once. What has run is recorded
in a `_toasty_migrations` table, so applying again is a no-op.

Toasty computes the diff and owns the file layout, the history and the tracking
table; this driver supplies only `generate_migration`. That is why the standard
[`toasty-cli`](https://crates.io/crates/toasty-cli) drives this driver
unchanged — a `ToastyCli` only needs a `Db`, and the `Db` is built with `Mssql`
rather than `connect`:

```rust,ignore
use toasty_cli::{Config, ToastyCli};

let driver = Mssql::from_url(
    "mssql://sa:Password1!@localhost:1433/mydb?encrypt=on&trust_certificate=true",
)?;

let db = toasty::Db::builder()
    .models(toasty::models!(crate::*))
    .build(driver)
    .await?;

let cli = ToastyCli::with_config(db, Config::load()?);
cli.parse_and_run().await?;
```

Put that in `src/bin/cli.rs` and the usual subcommands are available:

```bash
cargo run --bin cli -- migration snapshot   # print the schema Toasty derives
cargo run --bin cli -- migration generate --name add_posts
cargo run --bin cli -- migration apply      # run the pending SQL, in a transaction
cargo run --bin cli -- migration drop       # remove a migration from the history
cargo run --bin cli -- migration reset      # drop everything and re-apply
```

`Config::load()` reads a `Toasty.toml`; `Config::new().migration(...)` builds the
same thing in code. `statement_breakpoints` (on by default) is what puts the
`-- #[toasty::breakpoint]` markers between statements that this driver splits on;
a hand-written migration may use `GO` instead.

`examples/migrations.rs` is a one-file version of the same setup: run it bare to
walk the snapshot → generate → apply → verify cycle, or with arguments to use it
as the CLI itself.

```bash
cargo run --example migrations
cargo run --example migrations -- migration snapshot
```

When migrations need to ship inside a single binary rather than be read from
disk, `toasty::embed_migrations!` compiles them in — that is Toasty's too, and
works here for the same reason:

```rust,ignore
static MIGRATIONS: toasty::migration::MigrationSet = toasty::embed_migrations!();

let report = MIGRATIONS.apply(&db).await?;
```

## How it works

Toasty names its SQL dialect with a closed enum (`Dialect::{Sqlite, Postgresql,
Mysql, MariaDb}`), and `toasty_sql::Serializer` has constructors for those four
only — so an out-of-tree driver can neither reuse Toasty's SQL generation nor
add a dialect. This crate therefore:

- implements the public `Driver` and `Connection` traits, which *are* an open
  extension point;
- renders T-SQL itself from the public `toasty_core::stmt` AST
  (`src/sql/render.rs`), including its own `CREATE TABLE`/`CREATE INDEX` emitter
  (`src/sql/ddl.rs`) and migration diff (`src/migration.rs`);
- declares a `Capability` built from `Capability::MYSQL` that names
  `Dialect::Mysql` purely as the query planner's "this backend speaks SQL"
  hint. That value is never handed to `toasty_sql`.

Everything above the driver — connection pooling, the executor, transactions,
the query engine and response handling — is Toasty's.

## Layout

| Path | Contents |
|---|---|
| `src/capability.rs` | The `Capability` the planner reads |
| `src/driver.rs` | `Mssql`: `connect`, `reset_db`, `generate_migration`, `execute_raw` |
| `src/connection.rs` | `exec`, `push_schema`, `applied_migrations`, transactions |
| `src/sql/render.rs` | The T-SQL renderer for the statement AST |
| `src/sql/ddl.rs` | `CREATE TABLE` / `CREATE INDEX` |
| `src/migration.rs` | Schema diff → T-SQL, via `toasty_sql`'s diff engine |
| `src/spatial/mod.rs` | `MssqlGeometry` / `MssqlGeography`, model-facing (`spatial` feature) |
| `src/datetime_offset.rs` | `MssqlDateTimeOffset`, model-facing (`datetimeoffset` feature) |
| `src/spatial/codec.rs` | SQL Server's spatial serialization, MS-SSCLRT |
| `src/funcs.rs` | The wire format that carries a function call through Toasty's AST |
| `src/funcs/json.rs` | `MssqlJson`: JSON functions (`funcs-json`) |
| `src/funcs/string.rs` | `MssqlStr`: string functions (`funcs-string`) |
| `src/funcs/math.rs` | `MssqlMath`: mathematical functions (`funcs-math`) |
| `src/funcs/date.rs` | `MssqlDate`: date and time functions (`funcs-date`) |
| `src/spatial/funcs.rs` | `MssqlSpatial`, `MssqlGeometryExt`, `MssqlGeographyExt` (`spatial`) |
| `src/tds/client.rs` | Connect, execute, cursor handling, error classification |
| `src/tds/params.rs` | Binding values, and `?` → `@pN` marker rewriting |
| `src/tds/decode.rs` | Decoding result values |
| `src/type_map.rs` | `db::Type` → T-SQL column types |
| `old-sqlx/` | The reference `sqlx-mssql-rs` driver (read-only; not a dependency) |

T-SQL specifics the renderer handles: `[bracket]` identifier quoting, `@pN`
bind parameters, `OFFSET … FETCH NEXT` for `LIMIT`, `OUTPUT INSERTED`/`DELETED`
for `RETURNING`, `IDENTITY(1,1)` (with generated columns dropped from the
insert target list), `MERGE` for upserts, `WITH` for common table expressions,
`OPENJSON` for collections, `1`/`0` for booleans, and comparing bare boolean
values to `1` because T-SQL has no boolean expression type.

## Testing

The repository's `compose.dev.yaml` provides SQL Server 2022, and `.dev.env`
tells the tests where it is: `DATABASE_URL`, whose database each test replaces
with one of its own, because `reset_db` drops and recreates whatever it is
pointed at.

```bash
docker compose -f compose.dev.yaml up -d db
cargo test

# Each feature is a category of built-in function; `funcs` is all four.
cargo test --features funcs

# The spatial tests need the feature that carries the types they exercise.
cargo test --features spatial
```

Each integration test uses its own database, because `reset_db` drops and
recreates the database and the tests run in parallel.

The unit tests cover the rendering, type mapping and codecs without a server;
the conformance suite covers everything it can reach. What is left over is the
handful of things neither can answer, and the targets below are for those.

| Target | What it covers |
|---|---|
| `--lib` | the renderer, type map, codec and marker rewriter, with no server |
| `--test suite` | the published conformance suite — everything it can reach |
| `--test migration` | that generated migration SQL runs, and applies atomically |
| `--test locking` | the row-lock hint's effect, with two connections |
| `--features spatial --test geo` | that SQL Server accepts bytes this encoder produced, and that every spatial method is accepted |
| `--features datetimeoffset --test datetimeoffset` | that the column is a `datetimeoffset`, that the offset survives the round trip, and that ordering it is chronological |
| `--features funcs --test funcs` | that SQL Server accepts every supplied function, and answers the right rows |

`tests/locking.rs` needs two connections: it holds a lock in one transaction and
has a second connection wait for it, which is the only way to show the hint does
what the renderer says. Its second test repeats the read without the hint as a
control.

`tests/geo.rs` compares this driver's bytes against `STGeomFromText`'s for the
same shape, because an encoder that is *self-consistently* wrong still
round-trips through its own decoder, and still decodes payloads captured from a
real server.

`tests/funcs.rs` leans on the server for the same reason, and a little harder: a
rendering test can only confirm the SQL matches what we wrote down, so
`DATEPART(col, day)` would pass it and fail at the server. One test therefore
runs *every* function once against a real database, because a function whose SQL
the server rejects has no answer to check. It found nine defects on its first
run.

`tests/suite.rs` runs the published
[`toasty-driver-integration-suite`](https://crates.io/crates/toasty-driver-integration-suite)
against the driver.

## Status

Working, covered by the integration tests and the published conformance suite
(`toasty-driver-integration-suite`, all 770 generated tests passing):

- connect, `push_schema`, `reset_db`
- migrations: a schema diff is rendered to T-SQL (`ALTER TABLE`, `CREATE TABLE`,
  `DROP`, `sp_rename`), applying one is transactional, and it is recorded in a
  `_toasty_migrations` table that `applied_migrations` reads back
- `INSERT` returning generated keys via `OUTPUT INSERTED`, `UPDATE`/`DELETE` via
  `OUTPUT DELETED`, and upserts as `MERGE`
- `SELECT` with filters, joins, subqueries, common table expressions, ordering,
  offset/limit pagination, and cursor (keyset) pagination in both directions
- transactions (commit, rollback, savepoints); commit/rollback-only modes are
  rejected with `unsupported_feature`
- scalar types: `bool`, all integer widths (including `u64` up to `i64::MAX`, as
  `DECIMAL(20, 0)`), `f32`/`f64`, `String`, `Vec<u8>`, `Uuid`, `DECIMAL`/
  `NUMERIC` (including `bigdecimal` and `rust_decimal`), and the `jiff` temporal
  types as `DATE`, `TIME` and `DATETIME2`
- `Vec<scalar>` fields and `#[document]` embeds, both stored as JSON text in an
  `NVARCHAR(MAX)` column — the same fallback MySQL and SQLite use. Membership is
  an `OPENJSON` enumeration forced to a binary collation, and a document leaf is
  read with `JSON_VALUE`, or `JSON_QUERY` for a leaf that is itself an object or
  array
- enums, as `NVARCHAR` plus a `CHECK ([col] IN (...))` constraint
- composite keys, including row-value `IN` lists and `IN` subqueries
- positional `column1`, `column2` aliases for derived tables
- network addresses (`IpInet`, `IpCidr`, `MacAddr6`, `MacAddr8`) as bounded
  `NVARCHAR`, because SQL Server has no such column type
- with the `spatial` feature, `geometry` and `geography` columns, including a
  conversion to and from `geo_types` (see [Spatial data](#spatial-data))
- with the `datetimeoffset` feature, `MssqlDateTimeOffset` in a `datetimeoffset`
  column, which keeps the offset and orders chronologically (see
  [Timezone-aware values](#timezone-aware-values))

Not implemented, each returning `unsupported_feature` rather than producing
wrong SQL:

- removing a value or an index from a collection (`vec_remove`, `vec_pop`,
  `vec_remove_at`): a JSON array has no in-place removal in T-SQL, and the
  text-splicing that append uses cannot express "every element equal to x"
- set operations (`UNION`, `INTERSECT`, `EXCEPT`), which nothing in Toasty 0.11
  can produce — there is no constructor for the AST node
- a native `json`/`jsonb` column type, which SQL Server does not have before
  2025
- `scan`
- native `ILIKE`

Notes:

- `OUTPUT` requires `INTO` on tables that have triggers.
- A unique index over a nullable column is created with `WHERE [col] IS NOT NULL`,
  because SQL Server treats `NULL`s as equal for uniqueness and Toasty's planner
  expects them to be distinct.
- `starts_with` and collection membership are compared with
  `COLLATE Latin1_General_BIN2`, because SQL Server's default collation is
  case-insensitive and Toasty's string predicates are not.
- There is no signed 8-bit type in T-SQL and `TINYINT` is unsigned, so `i8`
  columns are `SMALLINT`.
- T-SQL has no collection `Intersects` or `IsSuperset` operator, so the
  capability reports `native_array_set_predicates: false` and the engine
  rewrites both into one membership test per element.
- A lock request timeout (SQL Server error 1222) is classified as a serialization
  failure, because the statement failed only for want of a lock and can succeed
  on a retry.
- `push_schema` creates tables and is not idempotent, like the other Toasty
  drivers; changing an existing database is what migrations are for. A `GO` line
  in a migration is not T-SQL — the client interprets it, so it is stripped
  before the batch is sent.
- `ping` and `is_valid` are both implemented, because the pool's health check is
  the only thing that can notice a connection which died while idle. What counts
  as "lost" is narrower than it sounds: `mssql-tds` reconnects before sending a
  batch if its transport has gone, so a session killed server-side is *recovered
  from* rather than reported, and `connection_lost` surfaces only when the
  reconnect cannot succeed either.
- Temporals are truncated to microseconds before binding, not rounded, so a bound
  value equals the ISO text a `#[document]` column stores, which is truncated
  too.
- A decimal parameter can arrive with no declared width (from a document leaf),
  so the driver derives `DECIMAL(p, s)` from the value's own digits.
- A document string leaf is **not** forced to a binary collation, unlike
  collection membership and `starts_with`: `JSON_VALUE` inherits the column's
  collation, and the suite requires the leaf to match with the same case
  sensitivity as a plain column.
- Documents are stored as text, so a filter on a document path has no index
  behind it: SQL Server indexes JSON paths only through a computed column.
- A `Zoned` value is stored as `NVARCHAR` text: Toasty's `db::Type` has no
  offset variant, and Toasty itself documents that a zoned value round-trips
  through text on every SQL backend, because no column type carries an IANA zone
  name. That is a hazard rather than a neutral fallback — an offset-bearing ISO
  8601 string does not sort or compare chronologically, so `ORDER BY` and range
  filters over the column are lexicographic and can disagree with the clock. For
  a temporal column use `jiff::Timestamp` (UTC, `DATETIME2(6)`), or
  `MssqlDateTimeOffset` when the offset itself matters (see
  [Timezone-aware values](#timezone-aware-values)).

## Spatial data

The `spatial` feature adds `MssqlGeometry` and `MssqlGeography`. Toasty has no
spatial type, but `toasty::schema::Field` is public and unsealed, so each of
these implements it: it reports `stmt::Type::Bytes`, and supplies its own
`geometry`/`geography` column type through `Field::field_ty`, which is why a
model only names the type.

```rust
use toasty_driver_mssql::{MssqlGeography, MssqlGeometry};

#[derive(toasty::Model)]
struct Place {
    #[key]
    id: i64,

    shape: MssqlGeometry,
    area: MssqlGeography,
}
```

SQL Server sends a spatial value as `varbinary` holding its own serialization
([MS-SSCLRT](https://learn.microsoft.com/en-us/openspecs/sql_server_protocols/ms-ssclrt/)),
which is not WKB. A value read from a column can be written straight back, and
`from_geometry` / `to_geometry` convert to and from a `geo_types::Geometry<f64>`
with an SRID, through `src/spatial/codec.rs`.

The two types are not interchangeable even though they share a payload format:
SQL Server reads the same ring the other way round for `geography`, so identical
bytes can be a triangle of half a square unit or the rest of the globe. That is
why the type, rather than a parameter, owns the column type.

`tests/geo.rs` checks the codec against the server rather than against itself —
it compares the bytes this driver produces with `geometry::STGeomFromText`'s for
the same shape, so an encoding that only *we* can read cannot pass.

Coordinates are `(x, y)`. For `geography` that means `x` is longitude and `y` is
latitude, the opposite order to the `(lat long)` SQL Server writes in its own
geography WKT: `Coord { x: 1.0, y: 2.0 }` reads back as the WKT's `(2 1)`. The
bytes are still exactly the server's.

Spatial *querying* works where the other shape is a literal:
`shape().st_distance("POINT(6 2)", 0)` renders
`[shape].STDistance(geometry::STGeomFromText(N'POINT(6 2)', 0))`.

## Timezone-aware values

`jiff::Zoned` is text on every Toasty backend, because no SQL column type carries
an IANA zone name — Toasty's own guide says so, and it rejects a `Zoned` leaf in a
`#[document]` rather than drop the annotation. Text storage has a cost the type
does not show: an offset-bearing ISO 8601 string compares and sorts by its
characters, not by the clock.

SQL Server *does* have a column type for "an instant and the offset it is
displayed in", so the `datetimeoffset` feature supplies the matching Rust type,
the same way `spatial` supplies the geometry ones:

```rust
use toasty_driver_mssql::MssqlDateTimeOffset;

#[derive(toasty::Model)]
struct Event {
    #[key]
    id: i64,

    happened_at: MssqlDateTimeOffset,
}
```

The column is a `datetimeoffset`, and `ORDER BY` over it is chronological.
`timestamp()` is the instant, `offset()` the offset it is displayed in, and
`to_zoned()` the underlying value; `from_zoned` and `from_timestamp` build one.

What it does **not** carry is the zone name: a `datetimeoffset` has a field for an
offset, not for a name, so `America/New_York` reads back as `-04:00`. That is why
this is a type of its own rather than a storage choice for `Zoned` — it never
claims the name, so nothing is silently lost. If the name matters, store it
alongside; if only the instant matters, `jiff::Timestamp` is simpler.

A `String` or `jiff::Timestamp` field can also name the column directly with
`#[column(type = "DATETIMEOFFSET")]`: a quoted name is a `db::Type::Custom`, so
the macro's compatibility check is skipped and the name is rendered verbatim. A
`Timestamp` then binds as `DATETIME2` and the server converts it, so the offset
is always `+00:00`. A `Zoned` field cannot use this — its `[IANA]`-tagged text is
rejected by the server's parser, which is what `MssqlDateTimeOffset` exists to
avoid.

## Scalar functions

Toasty's AST has no node for "call a function by name", and it is a closed
enum, so a driver cannot add one. What it does have is the node Toasty uses for
`#[document]` paths — which carries *both* an operand expression and a
`Vec<String>`. That node is borrowed: the vector names the SQL function where a
document path would go, and the renderer emits a call instead of an extraction.
Three details carry the shape:

* `!` says the path is a call and not a document path. It cannot collide with a
  real one, because those are Rust field names and `!` is not permitted in an
  identifier.
* `{base}` marks *where* the operand goes, because T-SQL is not consistent about
  it: `ISJSON(col, ARRAY)` takes its subject first, `DATEPART(day, col)` and
  `CHARINDEX(N'a', col)` take it last.
* `!.` marks a *method*, which is how T-SQL spells its whole spatial vocabulary
  and the only way it spells any of it: `col.STArea()`.

The functions are grouped by Microsoft's own [categories][cats], one Cargo
feature each, with `funcs` turning on all four. Spatial methods ride with
`spatial`, since they need the types it defines.

| Feature | Trait | Category |
|---|---|---|
| `funcs-json` | `MssqlJson` | JSON Functions |
| `funcs-string` | `MssqlStr` | String Functions |
| `funcs-math` | `MssqlMath` | Mathematical Functions |
| `funcs-date` | `MssqlDate` | Date and Time Functions |
| `spatial` | `MssqlSpatial` | the methods both spatial types share |
| `spatial` | `MssqlGeometryExt` | the methods only `geometry` has |
| `spatial` | `MssqlGeographyExt` | the method only `geography` has |

[cats]: https://learn.microsoft.com/en-us/sql/t-sql/functions/functions

```rust
use toasty_driver_mssql::{DatePart, MssqlDate as _, MssqlJson as _, MssqlMath as _, MssqlStr as _};
use toasty_driver_mssql::{MssqlSpatial as _, lit};

// A document column holding a JSON array, not an object.
Widget::filter(Widget::fields().notes().is_json_array())

// String columns.
Widget::filter(Widget::fields().notes().len().gt(80).or(!Widget::fields().notes().is_json()))

// A value-returning function is compared with the ordinary operators.
Widget::filter(Widget::fields().notes().char_index("urgent").gt(0))
Widget::filter(Widget::fields().created().date_add(DatePart::Day, 7).gt(Widget::fields().created()))

// Any call can be the receiver of another, so a result can be read further.
Widget::filter(Widget::fields().notes().upper().char_index("URGENT").gt(0))
Widget::filter(Widget::fields().created().date_add(DatePart::Year, 1).year().eq(2026))

// Every spatial method hangs off its column, and a two-shape method takes
// well-known text, wrapped in the constructor for that column's own type.
Widget::filter(Widget::fields().shape().st_intersects("POINT(0.5 0.5)", 0))

// A method that answers a *shape* is only useful chained: a geometry has no
// `=` operator, so the measurement is what says anything about it.
Widget::filter(Widget::fields().shape().st_union("POLYGON((…))", 0).st_area().gt(lit(30.0)))
```

81 methods are supplied: 8 JSON, 21 string, 11 mathematical, 11 temporal, and 30
spatial — 20 on both spatial types, 9 `geometry` only, 1 `geography` only. The
JSON ones matter most here, because this driver has no native JSON column type —
a `Vec<scalar>` or `#[document]` column is `NVARCHAR(MAX)` text with no
constraint on it at all, and `ISJSON` is the integrity check it does not
otherwise have.

`geography` has a **reduced OGC surface**: it has no `STTouches`, `STCrosses`
or `STIsSimple` — the server rejects each with "Could not find method … for type
SqlGeography" — and it spells its bounding box `EnvelopeCenter`/`EnvelopeAngle`
rather than `STEnvelope`. Its values are always valid, so it has no `MakeValid`
either. The three sets are separate traits for exactly that reason, and the
breadth test is what drew the line.

Four limits are worth knowing before using them, and the first is not ours:

- **A float constant on the other side of a call must be wrapped in `lit()`.**
  `price().sqrt().lt(4.0)` panics; `price().sqrt().lt(lit(4.0))` does not. The
  engine's bind pass infers a parameter's type from the expression opposite it,
  and it has no rule for `ExprFunc` — so the type must come from the value, and
  `f32`/`f64`/`BigDecimal`/`Zoned` values are the ones it cannot classify.
  Integers, strings, booleans, dates, timestamps, UUIDs and byte strings are
  unaffected, as is comparing against another column.
- **Filters only.** The engine drops a non-path expression in a projection, so a
  function cannot appear in `select`; it has to stand in a `WHERE`.
- **One expression operand, and nothing untrusted in the others.** Every other
  argument travels as escaped text, so no method takes two columns —
  `col_a.STDistance(col_b)` still needs raw SQL — and no argument can be a bind
  parameter.
- **`JSON_VALUE` and `JSON_QUERY` raise on invalid JSON** rather than returning
  NULL. Guarding them with `is_json` is not reliable, because SQL Server does not
  promise to evaluate the two in that order.

The real fix is upstream: an `ExprFunc::Custom(FuncCustom { name, args, ret })`
would remove every one of these limits, and the capability-gated-operator
pattern the codebase already uses for `native_starts_with` and
`predicate_match_any` is the natural place to describe it.

## Licensing

`old-sqlx/` is a copy of the Apache-2.0 licensed `sqlx-mssql-rs` driver and is
used only as a reference. Two files are adapted from it and say so: the `?`
marker rewriter in `src/tds/params.rs`, and `src/spatial/codec.rs`, which is a
format definition rather than a design choice and keeps its original shape.
