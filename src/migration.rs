//! Rendering schema diffs as T-SQL.
//!
//! Toasty diffs two schemas itself and hands the driver an ordered list of DDL
//! statements ([`toasty_sql::migration::MigrationStatement`]), so the dialect
//! work left here is a renderer for that small DDL AST. Reusing the diff engine
//! rather than reimplementing it keeps this driver's migrations in step with
//! the built-in drivers, including the decisions about which changes can be made
//! in place and which need a table copy.
//!
//! T-SQL cannot express a few of the things the DDL AST can, and those cases
//! return an error rather than rendering something that silently does less than
//! the migration asked for.

use toasty_core::{
    Error, Result,
    schema::{db, diff},
    stmt,
};
use toasty_sql::{migration::MigrationStatement, stmt as ddl};

use crate::{capability::MSSQL, sql::ddl as render_ddl, sql::render::quote_ident, type_map};

/// The marker [`toasty_core::schema::db::Migration::new_sql_with_breakpoints`]
/// separates statements with, and that `apply_migration` splits on.
pub(crate) const BREAKPOINT: &str = "-- #[toasty::breakpoint]";

/// Splits a migration into the batches that can be sent to the server.
///
/// Two separators apply, and neither is a semicolon:
///
/// * [`BREAKPOINT`], which is what this driver's own `generate_migration` writes
///   between statements. `;` cannot be used because it can appear inside a string
///   literal or a trigger body, and splitting on it would cut a statement in
///   half.
/// * `GO` on a line of its own, which is how SQL Server tooling separates batches
///   and which a hand-written migration will therefore contain. `GO` is not
///   T-SQL — it is interpreted by the client — so sending it to the server fails
///   with "Could not find stored procedure 'GO'".
///
/// No attempt is made to parse the SQL, so a `GO` line inside a multi-line string
/// literal or a block comment would be taken for a separator. Tooling does not
/// emit that, and the alternative is a lexer for a dialect this driver does not
/// otherwise need to understand.
pub(crate) fn batches(sql: &str) -> Vec<String> {
    let mut batches: Vec<String> = Vec::new();
    let mut current = String::new();

    let flush = |current: &mut String, batches: &mut Vec<String>| {
        let batch = current.trim();
        if !batch.is_empty() {
            batches.push(batch.to_owned());
        }
        current.clear();
    };

    for statement in sql.split(BREAKPOINT) {
        for line in statement.lines() {
            if line.trim().eq_ignore_ascii_case("GO") {
                flush(&mut current, &mut batches);
            } else {
                current.push_str(line);
                current.push('\n');
            }
        }

        // A breakpoint separates statements, so it ends the current batch even
        // when the next one does not open with `GO`.
        flush(&mut current, &mut batches);
    }

    batches
}

/// Renders a schema diff as T-SQL statements, in the order they must run.
pub(crate) fn render_diff(diff: &diff::Schema<'_>) -> Result<Vec<String>> {
    MigrationStatement::from_diff(diff, &MSSQL)
        .iter()
        .map(|step| render_step(step.schema(), step.statement()))
        .collect()
}

/// A migration that fails when it is applied.
///
/// `Driver::generate_migration` cannot return an error, and applying a comment
/// would look like success and be recorded as such, so an unrenderable diff
/// becomes SQL that raises with the reason instead.
pub(crate) fn unrenderable(error: &Error) -> String {
    let reason = error.to_string().replace('\n', " ");

    format!(
        "-- The SQL Server driver cannot render this migration.\n\
         THROW 50000, N'{}', 1;",
        literal(&format!("cannot render this migration: {reason}"))
    )
}

/// Renders one DDL statement against the schema snapshot it belongs to.
fn render_step(schema: &db::Schema, statement: &ddl::Statement) -> Result<String> {
    let sql = match statement {
        // These name their table (or index) by id, so the renderers
        // `push_schema` already uses do the work.
        ddl::Statement::CreateTable(create) => {
            render_ddl::create_table_statement(schema, schema.table(create.table))?
        }
        ddl::Statement::CreateIndex(create) => {
            let index = schema.table(create.on).indices[create.index.index].clone();
            render_ddl::create_index_statement(schema, &index)?
        }
        ddl::Statement::DropTable(drop) => format!(
            "DROP TABLE {}{}",
            if drop.if_exists { "IF EXISTS " } else { "" },
            quoted(&drop.name)
        ),
        ddl::Statement::DropIndex(drop) => {
            // T-SQL needs the table to drop an index from, and the statement
            // only carries the index name, so find it in the snapshot the step
            // was built against (the schema the index still exists in).
            let index = raw_name(&drop.name);
            let table = schema
                .tables
                .iter()
                .find(|table| table.indices.iter().any(|found| found.name == index))
                .ok_or_else(|| {
                    unsupported(format!(
                        "the migration drops index `{index}`, which is not in the schema it \
                         was generated against, and T-SQL needs the table to drop it from"
                    ))
                })?;

            format!(
                "DROP INDEX {}{} ON {}",
                if drop.if_exists { "IF EXISTS " } else { "" },
                quote_ident(&index),
                quote_ident(&table.name)
            )
        }
        ddl::Statement::AddColumn(add) => format!(
            "ALTER TABLE {} ADD {}",
            quote_ident(&schema.table(add.table).name),
            column_definition(&add.column)?
        ),
        ddl::Statement::AlterColumn(alter) => return render_alter_column(schema, alter),
        ddl::Statement::DropColumn(drop) => {
            let table = schema.table(drop.table).name.clone();
            let column = raw_name(&drop.name);

            // T-SQL has no `DROP COLUMN IF EXISTS`, so the guard is spelled out.
            let statement = format!(
                "ALTER TABLE {} DROP COLUMN {}",
                quote_ident(&table),
                quote_ident(&column)
            );

            if drop.if_exists {
                format!(
                    "IF COL_LENGTH(N'{}', N'{}') IS NOT NULL {}",
                    literal(&table),
                    literal(&column),
                    statement
                )
            } else {
                statement
            }
        }
        ddl::Statement::AlterTable(alter) => {
            let ddl::AlterTableAction::RenameTo(new_name) = &alter.action;

            format!(
                "EXEC sp_rename N'{}', N'{}'",
                literal(&raw_name(&alter.name)),
                literal(&raw_name(new_name))
            )
        }
        // Data migrations carry an ordinary statement, so the statement
        // renderer handles them.
        ddl::Statement::Query(query) => {
            crate::sql::render::render(schema, &stmt::Statement::Query(query.clone()))?
        }
        ddl::Statement::Insert(insert) => {
            crate::sql::render::render(schema, &stmt::Statement::Insert(insert.clone()))?
        }
        ddl::Statement::Update(update) => {
            crate::sql::render::render(schema, &stmt::Statement::Update(update.clone()))?
        }
        ddl::Statement::Delete(delete) => {
            crate::sql::render::render(schema, &stmt::Statement::Delete(delete.clone()))?
        }
        other => {
            return Err(unsupported(format!(
                "the SQL Server driver cannot render a `{other:?}` migration statement"
            )));
        }
    };

    Ok(sql)
}

/// Renders `ALTER TABLE … ALTER COLUMN`, or the `sp_rename` a rename needs.
fn render_alter_column(schema: &db::Schema, alter: &ddl::AlterColumn) -> Result<String> {
    let table = quote_ident(&schema.table(alter.id.table).name);
    let changes = &alter.changes;

    if let Some(new_name) = &changes.new_name {
        return Ok(format!(
            "EXEC sp_rename N'{}.{}', N'{}', N'COLUMN'",
            literal(&raw_table(&alter.id, schema)),
            literal(&alter.column_def.name),
            literal(new_name)
        ));
    }

    if changes.new_auto_increment.is_some() {
        // `IDENTITY` is fixed at table creation; changing it means rebuilding
        // the column, which is not something to do silently inside a migration.
        return Err(unsupported(format!(
            "T-SQL cannot change the identity property of `{}` in place",
            alter.column_def.name
        )));
    }

    // A type change may carry the nullability with it; a nullability-only
    // change arrives with no new type, and `ALTER COLUMN` always restates the
    // whole column, so the current type fills in.
    let ty = match &changes.new_ty {
        Some(ty) => ty.clone(),
        None => alter.column_def.ty.clone(),
    };
    let not_null = changes.new_not_null.unwrap_or(alter.column_def.not_null);

    Ok(format!(
        "ALTER TABLE {table} ALTER COLUMN {} {}",
        quote_ident(&alter.column_def.name),
        column_type(&ty, not_null, false)?
    ))
}

/// Renders a column definition, as `CREATE TABLE`, `ADD COLUMN` and
/// `ALTER COLUMN` all spell it.
fn column_definition(column: &ddl::ColumnDef) -> Result<String> {
    let mut sql = format!(
        "{} {}",
        quote_ident(&column.name),
        column_type(&column.ty, column.not_null, column.auto_increment)?
    );

    // The diff engine rewrites an enum column to text plus a CHECK constraint
    // for backends with no native enum type, which is this one.
    if let Some(check) = &column.check {
        sql.push_str(" CHECK (");
        sql.push_str(&check_expr(&check.expr)?);
        sql.push(')');
    }

    Ok(sql)
}

/// Renders the storage type with its `IDENTITY` and nullability clauses.
fn column_type(ty: &db::Type, not_null: bool, auto_increment: bool) -> Result<String> {
    let mut sql = type_map::column_type(ty)?;

    // `IDENTITY` implies `NOT NULL`, and T-SQL requires it first.
    if auto_increment {
        sql.push_str(" IDENTITY(1,1)");
    }

    if not_null {
        sql.push_str(" NOT NULL");
    }

    Ok(sql)
}

/// Renders the expression of a `CHECK` constraint.
///
/// Only the shape the diff engine builds is supported: an enum's variants as
/// `[col] IN (N'a', N'b')`.
fn check_expr(expr: &stmt::Expr) -> Result<String> {
    let stmt::Expr::InList(in_list) = expr else {
        return Err(unsupported(format!(
            "the SQL Server driver cannot render the check constraint {expr:?}"
        )));
    };

    let stmt::Expr::Ident(column) = in_list.expr.as_ref() else {
        return Err(unsupported(format!(
            "the SQL Server driver expected a column on the left of a check constraint, found {:?}",
            in_list.expr
        )));
    };

    let stmt::Expr::List(items) = in_list.list.as_ref() else {
        return Err(unsupported(format!(
            "the SQL Server driver expected a literal list in a check constraint, found {:?}",
            in_list.list
        )));
    };

    let variants = items
        .items
        .iter()
        .map(|item| match item {
            stmt::Expr::Value(stmt::Value::String(variant)) => {
                Ok(format!("N'{}'", literal(variant)))
            }
            other => Err(unsupported(format!(
                "the SQL Server driver expected a string enum variant, found {other:?}"
            ))),
        })
        .collect::<Result<Vec<_>>>()?
        .join(", ");

    Ok(format!("{} IN ({variants})", quote_ident(column)))
}

/// The table a column belongs to, by name.
fn raw_table(id: &db::ColumnId, schema: &db::Schema) -> String {
    schema.table(id.table).name.clone()
}

/// A possibly qualified name, with every part quoted.
fn quoted(name: &ddl::Name) -> String {
    name.0
        .iter()
        .map(|part| quote_ident(part))
        .collect::<Vec<_>>()
        .join(".")
}

/// The last part of a name, unquoted: what `sp_rename` and `COL_LENGTH` want.
fn raw_name(name: &ddl::Name) -> String {
    name.0.last().cloned().unwrap_or_default()
}

/// Escapes a string for use inside a T-SQL literal.
fn literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn unsupported(message: impl Into<String>) -> Error {
    Error::unsupported_feature(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use toasty_core::schema::diff::RenameHints;

    #[test]
    fn splits_a_migration_on_breakpoints_and_go() {
        // The breakpoint is what this driver writes between its own statements.
        let generated =
            format!("CREATE TABLE [a] ([id] BIGINT){BREAKPOINT}CREATE TABLE [b] ([id] BIGINT)");
        assert_eq!(
            batches(&generated),
            [
                "CREATE TABLE [a] ([id] BIGINT)",
                "CREATE TABLE [b] ([id] BIGINT)"
            ]
        );

        // `GO` is what a hand-written migration uses, so it has to be accepted
        // too — but it is not T-SQL and must not reach the server.
        let hand_written =
            "CREATE TABLE [a] ([id] BIGINT);\nGO\nCREATE TABLE [b] ([id] BIGINT);\nGO\n";
        assert_eq!(
            batches(hand_written),
            [
                "CREATE TABLE [a] ([id] BIGINT);",
                "CREATE TABLE [b] ([id] BIGINT);"
            ]
        );

        // `GO` is case-insensitive and tolerated with surrounding whitespace,
        // and both separators can appear in one migration.
        let mixed = format!("SELECT 1\ngo\nSELECT 2{BREAKPOINT}\n  Go  \nSELECT 3");
        assert_eq!(batches(&mixed), ["SELECT 1", "SELECT 2", "SELECT 3"]);

        assert!(batches("").is_empty());
        assert!(batches("\n\nGO\n\n").is_empty());
    }

    #[test]
    fn keeps_a_semicolon_inside_a_statement() {
        // The reason `;` is not the separator: it is data inside a literal, and
        // a split on it would cut the statement in half.
        let sql = "INSERT INTO [a] ([x]) VALUES (N'; -- not a breakpoint')";
        assert_eq!(batches(sql), [sql]);

        // A `GO` that is not alone on its line is part of the statement too.
        let sql = "SELECT N'GO' AS [x]";
        assert_eq!(batches(sql), [sql]);
    }

    fn column(
        table: usize,
        index: usize,
        name: &str,
        storage_ty: db::Type,
        nullable: bool,
    ) -> db::Column {
        db::Column {
            id: db::ColumnId {
                table: db::TableId(table),
                index,
            },
            name: name.to_owned(),
            ty: stmt::Type::String,
            storage_ty,
            nullable,
            primary_key: index == 0,
            auto_increment: false,
            versionable: false,
        }
    }

    /// `items(id BIGINT, name NVARCHAR(255))`.
    fn schema() -> db::Schema {
        db::Schema {
            tables: vec![db::Table {
                id: db::TableId(0),
                name: "items".to_owned(),
                columns: vec![
                    column(0, 0, "id", db::Type::Integer(8), false),
                    column(0, 1, "name", db::Type::VarChar(255), false),
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

    fn statements(previous: &db::Schema, next: &db::Schema) -> Vec<String> {
        let hints = RenameHints::default();
        let diff = diff::Schema::from(previous, next, &hints);

        render_diff(&diff).unwrap()
    }

    /// The end-to-end shape: a new schema against nothing creates the table.
    #[test]
    fn creating_a_table_from_nothing() {
        let empty = db::Schema { tables: Vec::new() };

        let sql = statements(&empty, &schema());

        assert_eq!(sql.len(), 1, "got: {sql:?}");
        assert!(sql[0].starts_with("CREATE TABLE [items] ("), "got: {sql:?}");
        assert!(
            sql[0].contains("[name] NVARCHAR(255) NOT NULL"),
            "got: {sql:?}"
        );
        assert!(sql[0].contains("PRIMARY KEY ([id])"), "got: {sql:?}");
    }

    #[test]
    fn adding_a_column() {
        let mut next = schema();
        next.tables[0]
            .columns
            .push(column(0, 2, "note", db::Type::Text, true));

        let sql = statements(&schema(), &next);

        assert_eq!(sql, vec!["ALTER TABLE [items] ADD [note] NVARCHAR(MAX)"]);
    }

    #[test]
    fn changing_a_column_type_restates_nullability() {
        let mut next = schema();
        let column = &mut next.tables[0].columns[1];
        column.storage_ty = db::Type::Text;
        column.nullable = true;

        let sql = statements(&schema(), &next);

        assert_eq!(
            sql,
            vec!["ALTER TABLE [items] ALTER COLUMN [name] NVARCHAR(MAX)"]
        );
    }

    #[test]
    fn dropping_a_table() {
        let empty = db::Schema { tables: Vec::new() };

        let sql = statements(&schema(), &empty);

        assert_eq!(sql, vec!["DROP TABLE [items]"]);
    }

    /// An enum column has no native type here, so the diff engine rewrites it
    /// to text and pins the variants with a check constraint — the same
    /// constraint `push_schema` writes from the storage type. This is the path
    /// where the migration renderer and the schema renderer have to agree.
    #[test]
    fn an_enum_column_gets_a_check_constraint() {
        let empty = db::Schema { tables: Vec::new() };
        let mut next = schema();
        next.tables[0].columns[1].storage_ty = db::Type::Enum(db::TypeEnum {
            name: None,
            variants: vec![
                db::EnumVariant {
                    name: "open".to_owned(),
                },
                db::EnumVariant {
                    name: "closed".to_owned(),
                },
            ],
        });

        let sql = statements(&empty, &next);

        assert_eq!(sql.len(), 1, "got: {sql:?}");
        assert!(
            sql[0].contains("CHECK ([name] IN (N'open', N'closed'))"),
            "got: {sql:?}"
        );
    }

    /// A column whose type is unchanged produces no statement at all.
    #[test]
    fn an_unchanged_schema_is_empty() {
        assert!(statements(&schema(), &schema()).is_empty());
    }
}
