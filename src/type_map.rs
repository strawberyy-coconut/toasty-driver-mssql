//! Mapping between Toasty's database types and T-SQL column types.
//!
//! The schema builder has already resolved each [`db::Column`]'s storage type
//! into [`db::Column::storage_ty`], so DDL only needs to render a [`db::Type`].
//! Application-level types still need bridging when a `CAST` is rendered.

use toasty_core::{Error, Result, schema::db, stmt};

use crate::capability::MSSQL;

/// Renders a [`db::Type`] as the T-SQL type used in DDL.
///
/// Returns [`Error::unsupported_feature`] for types the driver does not render
/// yet, so an unsupported model fails at schema build time with a clear message
/// rather than producing invalid SQL.
pub(crate) fn column_type(ty: &db::Type) -> Result<String> {
    let text = match ty {
        db::Type::Boolean => "BIT".to_owned(),

        // SQL Server's integer types are all signed, so an unsigned width uses
        // the next signed width that can hold it (u8 -> SMALLINT, and so on).
        db::Type::Integer(width) => integer_type(*width)?,
        db::Type::UnsignedInteger(width) => unsigned_integer_type(*width)?,

        db::Type::Float(width) => match width {
            4 => "REAL".to_owned(),
            8 => "FLOAT".to_owned(),
            other => {
                return Err(Error::unsupported_feature(format!(
                    "SQL Server driver does not support a {other}-byte float"
                )));
            }
        },

        // `NVARCHAR` rather than `VARCHAR` so every string round-trips Unicode
        // without depending on the database's collation.
        db::Type::Text => "NVARCHAR(MAX)".to_owned(),
        db::Type::VarChar(len) => format!("NVARCHAR({len})"),

        db::Type::Uuid => "UNIQUEIDENTIFIER".to_owned(),
        db::Type::Blob => "VARBINARY(MAX)".to_owned(),
        db::Type::Binary(len) => format!("BINARY({len})"),

        db::Type::Numeric(precision) => match precision {
            Some((precision, scale)) => format!("DECIMAL({precision}, {scale})"),
            None => {
                return Err(Error::unsupported_feature(
                    "SQL Server requires an explicit precision and scale for a decimal column; \
                     use `#[column(type = decimal(p, s))]`",
                ));
            }
        },

        // Not `TIMESTAMP`, which in T-SQL is a synonym for `ROWVERSION`.
        db::Type::Timestamp(precision) => format!("DATETIME2({precision})"),
        db::Type::DateTime(precision) => format!("DATETIME2({precision})"),
        db::Type::Date => "DATE".to_owned(),
        db::Type::Time(precision) => format!("TIME({precision})"),

        // Toasty has no native enum type on this backend, so the variant name is
        // stored in a string column and restricted with a CHECK constraint (see
        // `crate::sql::ddl`). The bounded default string type keeps indexed enum
        // columns indexable.
        db::Type::Enum(_) => column_type(&MSSQL.storage_types.default_string_type)?,

        // No native array type before SQL Server 2025, so a `Vec<scalar>` column
        // is JSON text, which is the fallback the MySQL and SQLite drivers use.
        // `NVARCHAR` rather than `VARCHAR` so string elements survive any
        // collation, and `MAX` because a bounded `NVARCHAR` would cap the
        // serialized array length. The element type is tracked by the engine; it
        // does not surface in the column DDL.
        db::Type::List(_) => "NVARCHAR(MAX)".to_owned(),

        // No native JSON type before SQL Server 2025 either, so a `#[document]`
        // column is JSON text — which is how SQL Server's own documentation
        // describes storing JSON ("JSON text is stored in varchar or nvarchar
        // columns and is indexed as plain text"). The JSON *functions* are what
        // make it usable, and they have been there since SQL Server 2016.
        db::Type::Document { .. } => "NVARCHAR(MAX)".to_owned(),
        db::Type::Json | db::Type::Jsonb => {
            return Err(Error::unsupported_feature(
                "SQL Server JSON columns are not supported by this driver yet",
            ));
        }
        db::Type::Cidr | db::Type::Inet | db::Type::MacAddr | db::Type::MacAddr8 => {
            return Err(Error::unsupported_feature(
                "SQL Server network address columns are not supported by this driver yet",
            ));
        }
        db::Type::Custom(name) => name.clone(),
    };

    Ok(text)
}

/// Renders the T-SQL type for an application-level [`stmt::Type`], used when a
/// `CAST` names a type rather than a column.
pub(crate) fn column_type_for(ty: &stmt::Type) -> Result<String> {
    let storage = storage_type(ty)?;
    column_type(&storage)
}

/// The database type an application-level type is stored as.
fn storage_type(ty: &stmt::Type) -> Result<db::Type> {
    let storage_types = &MSSQL.storage_types;

    let ty = match ty {
        stmt::Type::Bool => db::Type::Boolean,
        stmt::Type::I8 => db::Type::Integer(1),
        stmt::Type::I16 => db::Type::Integer(2),
        stmt::Type::I32 => db::Type::Integer(4),
        stmt::Type::I64 => db::Type::Integer(8),
        stmt::Type::U8 => db::Type::UnsignedInteger(1),
        stmt::Type::U16 => db::Type::UnsignedInteger(2),
        stmt::Type::U32 => db::Type::UnsignedInteger(4),
        stmt::Type::U64 => db::Type::UnsignedInteger(8),
        stmt::Type::F32 => db::Type::Float(4),
        stmt::Type::F64 => db::Type::Float(8),
        stmt::Type::String => storage_types.default_string_type.clone(),
        stmt::Type::Bytes => storage_types.default_bytes_type.clone(),
        stmt::Type::Uuid => storage_types.default_uuid_type.clone(),
        // A decimal without a declared precision and scale is stored as text,
        // which is what the storage types say.
        stmt::Type::Decimal => storage_types.default_decimal_type.clone(),
        stmt::Type::BigDecimal => storage_types.default_bigdecimal_type.clone(),
        // No native network address types, so each is the bounded text the
        // storage types name.
        stmt::Type::Cidr => storage_types.default_cidr_type.clone(),
        stmt::Type::Inet => storage_types.default_inet_type.clone(),
        stmt::Type::MacAddr => storage_types.default_macaddr_type.clone(),
        stmt::Type::MacAddr8 => storage_types.default_macaddr8_type.clone(),
        stmt::Type::Timestamp => storage_types.default_timestamp_type.clone(),
        stmt::Type::Zoned => storage_types.default_zoned_type.clone(),
        stmt::Type::Date => storage_types.default_date_type.clone(),
        stmt::Type::Time => storage_types.default_time_type.clone(),
        stmt::Type::DateTime => storage_types.default_datetime_type.clone(),
        // A list of scalars is stored as JSON text; `db::Type::list` collapses a
        // list of documents into one document, which is the engine's invariant.
        stmt::Type::List(elem) => db::Type::list(storage_type(elem)?),
        other => {
            return Err(Error::unsupported_feature(format!(
                "SQL Server driver cannot store a column of type {other:?}"
            )));
        }
    };

    Ok(ty)
}

/// Renders a signed integer of the given byte width.
///
/// There is no signed 8-bit type in T-SQL, and `TINYINT` is unsigned, so the
/// 1-byte width widens to `SMALLINT`. Writing `-1` into a `TINYINT` would store
/// 255 instead, which is how the round trip used to break.
fn integer_type(width: u8) -> Result<String> {
    let text = match width {
        1 => "SMALLINT",
        2 => "SMALLINT",
        4 => "INT",
        8 => "BIGINT",
        other => {
            return Err(Error::unsupported_feature(format!(
                "SQL Server driver does not support a {other}-byte integer"
            )));
        }
    };

    Ok(text.to_owned())
}

/// Renders an unsigned integer of the given byte width.
///
/// SQL Server's integer types are all signed, so each width promotes to the next
/// signed width that can hold it. A 64-bit unsigned value rides `DECIMAL(20, 0)`,
/// which holds the full `0..=2^64-1` range.
fn unsigned_integer_type(width: u8) -> Result<String> {
    let text = match width {
        1 => "SMALLINT",
        2 => "INT",
        4 => "BIGINT",
        8 => "DECIMAL(20, 0)",
        other => {
            return Err(Error::unsupported_feature(format!(
                "SQL Server driver does not support a {other}-byte unsigned integer"
            )));
        }
    };

    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_scalar_types() {
        assert_eq!(column_type(&db::Type::Boolean).unwrap(), "BIT");
        assert_eq!(column_type(&db::Type::Integer(8)).unwrap(), "BIGINT");
        assert_eq!(
            column_type(&db::Type::UnsignedInteger(8)).unwrap(),
            "DECIMAL(20, 0)"
        );
        assert_eq!(column_type(&db::Type::Float(8)).unwrap(), "FLOAT");
        assert_eq!(column_type(&db::Type::Text).unwrap(), "NVARCHAR(MAX)");
        assert_eq!(
            column_type(&db::Type::VarChar(4000)).unwrap(),
            "NVARCHAR(4000)"
        );
        assert_eq!(column_type(&db::Type::Uuid).unwrap(), "UNIQUEIDENTIFIER");
        assert_eq!(column_type(&db::Type::Blob).unwrap(), "VARBINARY(MAX)");
        assert_eq!(column_type(&db::Type::DateTime(6)).unwrap(), "DATETIME2(6)");
        assert_eq!(column_type(&db::Type::Time(6)).unwrap(), "TIME(6)");
        assert_eq!(column_type(&db::Type::Date).unwrap(), "DATE");
        assert_eq!(
            column_type(&db::Type::Numeric(Some((38, 10)))).unwrap(),
            "DECIMAL(38, 10)"
        );
    }

    #[test]
    fn rejects_unwired_types() {
        assert!(column_type(&db::Type::Json).is_err());
        assert!(column_type(&db::Type::Numeric(None)).is_err());
    }

    #[test]
    fn renders_a_document_as_json_text() {
        // SQL Server has no native JSON type, and its own documentation stores
        // JSON as `nvarchar` text; the JSON functions are what make it usable.
        assert_eq!(
            column_type(&db::Type::Document { binary: false }).unwrap(),
            "NVARCHAR(MAX)"
        );
        assert_eq!(
            column_type(&db::Type::Document { binary: true }).unwrap(),
            "NVARCHAR(MAX)"
        );
    }

    #[test]
    fn renders_a_scalar_collection_as_json_text() {
        assert_eq!(
            column_type(&db::Type::List(Box::new(db::Type::Integer(8)))).unwrap(),
            "NVARCHAR(MAX)"
        );
        assert_eq!(
            column_type_for(&stmt::Type::List(Box::new(stmt::Type::String))).unwrap(),
            "NVARCHAR(MAX)"
        );
        // A list of documents is one document, never a nested list, which is the
        // engine's own invariant (`db::Type::list`).
        assert_eq!(
            db::Type::list(db::Type::Document { binary: true }),
            db::Type::Document { binary: true }
        );
    }
}
