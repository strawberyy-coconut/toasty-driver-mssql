//! The driver's [`Capability`] description.
//!
//! Toasty's [`Dialect`] enum has no SQL Server variant and is not
//! `#[non_exhaustive]`, so there is no way for an out-of-tree driver to register
//! T-SQL as a dialect. The driver therefore names [`Dialect::Mysql`] purely as
//! the query planner's "this backend speaks SQL" hint, and renders T-SQL itself.
//! The dialect value is never handed to `toasty_sql`.

use toasty_core::{
    driver::{Capability, Dialect, SchemaMutations, SqlPlaceholder, StorageTypes},
    schema::db,
};

/// SQL Server storage types.
///
/// `db::Type` has no `NVarChar` variant, so `VarChar(n)` is rendered as
/// `NVARCHAR(n)` (see [`crate::type_map`]). `NVARCHAR` is bounded and
/// indexable, unlike `NVARCHAR(MAX)`, which is what makes
/// [`default_string_type`](StorageTypes::default_string_type) a bounded type
/// rather than `Text`.
const MSSQL_STORAGE_TYPES: StorageTypes = StorageTypes {
    // `NVARCHAR(4000)` — the largest non-`MAX` `NVARCHAR`. Bounded so that
    // indexed and unique string columns are indexable at all; SQL Server
    // cannot index `NVARCHAR(MAX)`.
    default_string_type: db::Type::VarChar(4000),

    // The maximum length an explicit varchar type may ask for.
    varchar: Some(4000),

    // SQL Server has a native 16-byte GUID type.
    default_uuid_type: db::Type::Uuid,

    // Rendered as `VARBINARY(MAX)`.
    default_bytes_type: db::Type::Blob,

    // SQL Server's `DECIMAL` requires an explicit precision and scale, which the
    // schema does not always carry. Store the untyped default as text, as the
    // MySQL driver does.
    default_decimal_type: db::Type::Text,
    default_bigdecimal_type: db::Type::Text,

    // `DATETIME2(6)` — microsecond precision, and unlike `TIMESTAMP`
    // (rowversion in T-SQL) it is a real date/time type.
    default_timestamp_type: db::Type::DateTime(6),

    // No native timezone-aware type is wired up yet, so a `Zoned` value is
    // stored as text, as the MySQL driver does.
    default_zoned_type: db::Type::Text,

    default_date_type: db::Type::Date,
    default_time_type: db::Type::Time(6),
    default_datetime_type: db::Type::DateTime(6),

    // SQL Server has no native network address types; bounded text keeps
    // indexes compact while fitting IPv6 prefixes and EUI-64.
    default_cidr_type: db::Type::VarChar(43),
    default_inet_type: db::Type::VarChar(43),
    default_macaddr_type: db::Type::VarChar(17),
    default_macaddr8_type: db::Type::VarChar(23),

    // SQL Server's integer types are all signed, so `u64` is capped at
    // `i64::MAX` rather than silently switching to `DECIMAL`.
    max_unsigned_integer: Some(i64::MAX as u64),
};

/// SQL Server capabilities.
///
/// Fields not overridden here are inherited from [`Capability::MYSQL`], the
/// closest SQL backend.
pub static MSSQL: Capability = Capability {
    driver_name: "MSSQL",

    // Planner hint only — T-SQL rendering lives in this crate, never in
    // `toasty_sql`.
    sql: Some(Dialect::Mysql),

    // The driver accepts `?`/`?N` and rewrites it to the `@pN` names
    // `sp_executesql` requires.
    sql_placeholder: Some(SqlPlaceholder::NumberedQuestionMark),

    storage_types: MSSQL_STORAGE_TYPES,

    // `ALTER TABLE ... ALTER COLUMN` changes the type and nullability in one
    // statement; renames go through `sp_rename`.
    schema_mutations: SchemaMutations {
        alter_column_type: true,
        alter_column_properties_atomic: true,
    },

    // T-SQL has `OUTPUT INSERTED.<cols>` on both INSERT and UPDATE, which is
    // what `RETURNING` lowers to. This also gives generated keys back without a
    // second round trip.
    returning_from_insert: true,
    returning_from_update: true,

    // `MERGE` covers all four shapes. Target matching is driven by the `ON`
    // clause the planner puts in the statement, so primary-key and unique
    // upserts are the same rendering. `HOLDLOCK` is required: without it the
    // engine can release the range lock between the match test and the write,
    // which is exactly the race an upsert exists to avoid.
    upsert_primary_key: true,
    upsert_unique: true,
    upsert_branch_assignments: true,
    upsert_targeted_ignore: true,

    // `IDENTITY(1,1)`.
    auto_increment: true,
    max_auto_increment_integer_width: None,

    // `sysname` is `nvarchar(128)`.
    max_identifier_length: Some(128),

    native_varchar: true,

    // `Expr::StartsWith` is rewritten by the planner into a `LIKE` pattern with
    // `%`, `_` and `!` escaped, which the renderer emits with `ESCAPE '!'`. The
    // equivalent SQLite/MySQL flags are off because neither the GLOB nor the
    // `BINARY ... LIKE` form exists in T-SQL.
    binary_like_starts_with: true,
    glob_starts_with: false,

    // T-SQL has `LIKE`; `ILIKE` is PostgreSQL-only.
    native_like: true,

    // The planner drives SQL databases through `QuerySql`; the key-value `Scan`
    // operation is not implemented.
    scan: false,

    // A previous page is the same query with the `ORDER BY` reversed and a
    // strict inequality on the cursor key, which T-SQL answers trivially. The
    // engine builds both cursors; the driver only has to run the query.
    backward_pagination: true,

    // No native JSON column type before SQL Server 2025, and no named enum
    // types. `Binary` storage is not wired up yet either.
    native_json: false,
    native_jsonb: false,
    native_enum: false,
    named_enum_types: false,

    // No native array type, so a `Vec<scalar>` column is JSON text, as on MySQL
    // and SQLite. `OPENJSON` supplies the element enumeration, which is enough
    // for membership, length and append — but a JSON array has no in-place
    // removal, so the three removal flags stay off.
    native_array: false,
    vec_scalar: true,
    unique_list_index: false,

    // A `#[document]` field is JSON text too, read back with `JSON_VALUE` and
    // `JSON_QUERY`. SQL Server has no index on a JSON path — one has to go
    // through a computed column — so path filters scan, but that is a property
    // of the queries rather than of this flag.
    document_collections: true,

    // A whole `Vec<scalar>` binds as one `NVARCHAR` parameter holding the JSON
    // text, so the extract pass must keep the list intact. Reporting `false`
    // instead expands the list into one argument per element, which renders as a
    // T-SQL row value in a scalar position.
    bind_list_param: true,

    // A JSON array has no set operator in T-SQL. Reporting this makes the engine
    // rewrite `Intersects`/`IsSuperset` with a concrete rhs into one membership
    // test per element, which is the `OPENJSON` form this driver does render.
    native_array_set_predicates: false,

    vec_remove: false,
    vec_pop: false,
    vec_remove_at: false,

    // SQL Server has native temporal and fixed-precision decimal types.
    native_timestamp: true,
    native_date: true,
    native_time: true,
    native_datetime: true,
    native_decimal: true,
    decimal_arbitrary_precision: false,
    bigdecimal_implemented: false,

    // No native network address types.
    native_cidr: false,
    native_inet: false,
    native_macaddr: false,
    native_macaddr8: false,

    test_connection_pool: true,

    // No SQLite-style lock-mode keyword; T-SQL locks through table hints.
    transaction_lock_mode: false,

    // `SELECT ... FOR UPDATE` is rendered as `WITH (UPDLOCK, ROWLOCK)`.
    select_for_update: true,

    // T-SQL has no `expr = ANY(<array>)` form: `ANY` only accepts a subquery.
    // Reporting this keeps the planner from producing one, which is what the
    // JSON operations are written against.
    predicate_match_any: false,

    ..Capability::MYSQL
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_is_self_consistent() {
        MSSQL.validate().expect("MSSQL capability must validate");
    }

    #[test]
    fn describes_mssql_not_mysql() {
        assert_eq!(MSSQL.driver_name, "MSSQL");
        assert_eq!(MSSQL.storage_types.default_uuid_type, db::Type::Uuid);
        assert_eq!(
            MSSQL.storage_types.max_unsigned_integer,
            Some(i64::MAX as u64)
        );
        assert!(MSSQL.returning_from_insert);
        assert!(MSSQL.returning_from_update);
        assert!(MSSQL.upsert_primary_key);
        assert!(MSSQL.upsert_unique);
        assert!(MSSQL.upsert_branch_assignments);
        assert!(MSSQL.upsert_targeted_ignore);
        assert!(MSSQL.sql());
    }
}
