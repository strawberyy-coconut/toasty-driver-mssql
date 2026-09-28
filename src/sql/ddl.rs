//! DDL generation for `push_schema` and `generate_migration`.
//!
//! Toasty's own DDL generator lives in `toasty_sql` and is dialect-gated, so an
//! out-of-tree driver emits its own.

use std::fmt::Write as _;

use toasty_core::{Result, schema::db, stmt};

use super::render::quote_ident;
use crate::type_map;

/// The statements that create a table and its secondary indices.
pub(crate) fn create_table(schema: &db::Schema, table: &db::Table) -> Result<Vec<String>> {
    let mut statements = vec![create_table_statement(schema, table)?];

    for index in &table.indices {
        if index.primary_key {
            // The primary key is part of the table definition, not a separate
            // index.
            continue;
        }

        statements.push(create_index_statement(schema, index)?);
    }

    Ok(statements)
}

/// Renders `CREATE TABLE`.
pub(crate) fn create_table_statement(schema: &db::Schema, table: &db::Table) -> Result<String> {
    let mut sql = String::new();
    write!(sql, "CREATE TABLE {} (", quote_ident(&table.name)).expect("string write");

    for column in &table.columns {
        let ty = type_map::column_type(&column.storage_ty)?;
        let name = quote_ident(&column.name);

        write!(sql, "\n    {name} {ty}").expect("string write");

        // `IDENTITY` implies `NOT NULL`, and T-SQL requires it before the
        // nullability clause.
        if column.auto_increment {
            sql.push_str(" IDENTITY(1,1)");
        }

        if !column.nullable {
            sql.push_str(" NOT NULL");
        }

        // Without a native enum type the variant name lives in a string column,
        // so the allowed values are pinned with a CHECK constraint.
        if let db::Type::Enum(type_enum) = &column.storage_ty {
            write!(sql, " {}", enum_check(&column.name, type_enum)).expect("string write");
        }

        sql.push(',');
    }

    // A composite primary key has to be declared as a table constraint.
    if !table.primary_key.columns.is_empty() {
        let columns = table
            .primary_key
            .columns
            .iter()
            .map(|id| quote_ident(&schema.column(*id).name))
            .collect::<Vec<_>>()
            .join(", ");

        write!(sql, "\n    PRIMARY KEY ({columns}),").expect("string write");
    }

    // The loop above leaves a trailing comma on the last entry.
    if sql.ends_with(',') {
        sql.pop();
    }

    sql.push_str("\n)");

    Ok(sql)
}

/// Renders the `CHECK` constraint that pins an enum column to its variants.
///
/// Shared with the migration renderer, which receives the same constraint in a
/// different shape (an expression rather than a storage type) because the diff
/// engine rewrites an enum column to text when the backend has no native enum.
pub(crate) fn enum_check(column: &str, type_enum: &db::TypeEnum) -> String {
    let variants = type_enum
        .variants
        .iter()
        .map(|variant| format!("N'{}'", variant.name.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");

    format!("CHECK ({} IN ({variants}))", quote_ident(column))
}

/// Renders `CREATE INDEX`.
pub(crate) fn create_index_statement(schema: &db::Schema, index: &db::Index) -> Result<String> {
    let mut sql = String::new();
    let unique = if index.unique { "UNIQUE " } else { "" };
    let table = schema.table(index.on);

    write!(
        sql,
        "CREATE {unique}INDEX {} ON {} (",
        quote_ident(&index.name),
        quote_ident(&table.name)
    )
    .expect("string write");

    for (position, column) in index.columns.iter().enumerate() {
        if position > 0 {
            sql.push_str(", ");
        }

        let name = quote_ident(&schema.column(column.column).name);

        match column.op {
            db::IndexOp::Eq => write!(sql, "{name} ASC").expect("string write"),
            db::IndexOp::Sort(stmt::Direction::Asc) => {
                write!(sql, "{name} ASC").expect("string write")
            }
            db::IndexOp::Sort(stmt::Direction::Desc) => {
                write!(sql, "{name} DESC").expect("string write")
            }
        }
    }

    sql.push(')');

    // SQL Server counts `NULL`s as equal when enforcing a unique index, so a
    // second row with a `NULL` in a nullable unique column is rejected. Every
    // other backend Toasty supports treats `NULL`s as distinct, and the planner
    // relies on that, so the index is filtered to the rows where the comparison
    // is actually meaningful. This is the documented T-SQL idiom for it.
    if index.unique {
        let comparable = index
            .columns
            .iter()
            .map(|column| schema.column(column.column))
            .filter(|column| column.nullable)
            .map(|column| format!("{} IS NOT NULL", quote_ident(&column.name)))
            .collect::<Vec<_>>();

        if !comparable.is_empty() {
            write!(sql, " WHERE {}", comparable.join(" AND ")).expect("string write");
        }
    }

    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> db::Schema {
        db::Schema {
            tables: vec![db::Table {
                id: db::TableId(0),
                name: "users".to_owned(),
                columns: vec![
                    db::Column {
                        id: db::ColumnId {
                            table: db::TableId(0),
                            index: 0,
                        },
                        name: "id".to_owned(),
                        ty: stmt::Type::I64,
                        storage_ty: db::Type::Integer(8),
                        nullable: false,
                        primary_key: true,
                        auto_increment: true,
                        versionable: false,
                    },
                    db::Column {
                        id: db::ColumnId {
                            table: db::TableId(0),
                            index: 1,
                        },
                        name: "name".to_owned(),
                        ty: stmt::Type::String,
                        storage_ty: db::Type::VarChar(4000),
                        nullable: false,
                        primary_key: false,
                        auto_increment: false,
                        versionable: false,
                    },
                ],
                primary_key: db::PrimaryKey {
                    columns: vec![db::ColumnId {
                        table: db::TableId(0),
                        index: 0,
                    }],
                    index: db::IndexId {
                        table: db::TableId(0),
                        index: 0,
                    },
                },
                indices: Vec::new(),
            }],
        }
    }

    #[test]
    fn renders_create_table() {
        let schema = schema();
        let table = schema.table(db::TableId(0));

        let sql = create_table_statement(&schema, table).unwrap();

        assert!(sql.starts_with("CREATE TABLE [users] ("), "got: {sql}");
        assert!(
            sql.contains("[id] BIGINT IDENTITY(1,1) NOT NULL"),
            "got: {sql}"
        );
        assert!(sql.contains("[name] NVARCHAR(4000) NOT NULL"), "got: {sql}");
        assert!(sql.contains("PRIMARY KEY ([id])"), "got: {sql}");
    }

    #[test]
    fn renders_unique_index() {
        let schema = schema();
        let index = db::Index {
            id: db::IndexId {
                table: db::TableId(0),
                index: 1,
            },
            name: "users_name_idx".to_owned(),
            on: db::TableId(0),
            columns: vec![db::IndexColumn {
                column: db::ColumnId {
                    table: db::TableId(0),
                    index: 1,
                },
                op: db::IndexOp::Eq,
                scope: db::IndexScope::Local,
            }],
            unique: true,
            primary_key: false,
        };

        let sql = create_index_statement(&schema, &index).unwrap();

        assert_eq!(
            sql,
            "CREATE UNIQUE INDEX [users_name_idx] ON [users] ([name] ASC)"
        );
    }

    #[test]
    fn filters_unique_index_on_nullable_columns() {
        let mut schema = schema();
        schema.tables[0].columns[1].nullable = true;

        let index = db::Index {
            id: db::IndexId {
                table: db::TableId(0),
                index: 1,
            },
            name: "users_name_idx".to_owned(),
            on: db::TableId(0),
            columns: vec![db::IndexColumn {
                column: db::ColumnId {
                    table: db::TableId(0),
                    index: 1,
                },
                op: db::IndexOp::Eq,
                scope: db::IndexScope::Local,
            }],
            unique: true,
            primary_key: false,
        };

        let sql = create_index_statement(&schema, &index).unwrap();

        assert_eq!(
            sql,
            "CREATE UNIQUE INDEX [users_name_idx] ON [users] ([name] ASC) WHERE [name] IS NOT NULL"
        );
    }
}
