//! Migrations, against a live server.
//!
//! A test cannot hold two versions of a model — the schema is derived from
//! `#[derive(Model)]` at compile time — so this drives the driver directly: build
//! a `db::Schema`, ask for the migration from nothing to it, and apply it.
//!
//! The rendering is already covered by the unit tests in `src/migration.rs`, and
//! `batches()` by its own. What only a server can answer is whether the SQL those
//! tests compare as strings actually *runs* — and whether applying it is really
//! all-or-nothing. Both of those have been wrong here before: the statements used
//! to be split on `;`, which cut a statement in half as soon as a literal
//! contained one.

use toasty_core::{
    driver::{ConnectContext, Driver as _},
    schema::{db, db::Migration, diff::RenameHints},
};
use toasty_driver_mssql::Mssql;

mod common;

fn column(index: usize, name: &str, storage_ty: db::Type, nullable: bool) -> db::Column {
    db::Column {
        id: db::ColumnId {
            table: db::TableId(0),
            index,
        },
        name: name.to_owned(),
        ty: toasty_core::stmt::Type::String,
        storage_ty,
        nullable,
        primary_key: index == 0,
        auto_increment: false,
        versionable: false,
    }
}

/// `items(id BIGINT PRIMARY KEY, name NVARCHAR(255) NOT NULL)`.
fn schema_v1() -> db::Schema {
    let id = db::ColumnId {
        table: db::TableId(0),
        index: 0,
    };

    db::Schema {
        tables: vec![db::Table {
            id: db::TableId(0),
            name: "items".to_owned(),
            columns: vec![
                column(0, "id", db::Type::Integer(8), false),
                column(1, "name", db::Type::VarChar(255), false),
            ],
            primary_key: db::PrimaryKey {
                columns: vec![id],
                index: db::IndexId {
                    table: db::TableId(0),
                    index: 0,
                },
            },
            // The primary key has to be a real index: the diff engine resolves
            // the primary key's index id.
            indices: vec![db::Index {
                id: db::IndexId {
                    table: db::TableId(0),
                    index: 0,
                },
                name: "items_pkey".to_owned(),
                on: db::TableId(0),
                columns: vec![db::IndexColumn {
                    column: id,
                    op: db::IndexOp::Eq,
                    scope: db::IndexScope::Local,
                }],
                unique: true,
                primary_key: true,
            }],
        }],
    }
}

fn empty() -> db::Schema {
    db::Schema { tables: Vec::new() }
}

fn diff<'a>(
    from: &'a db::Schema,
    to: &'a db::Schema,
    hints: &'a RenameHints,
) -> toasty_core::schema::diff::Schema<'a> {
    toasty_core::schema::diff::Schema::from(from, to, hints)
}

/// A migration is all or nothing, and statements are separated by the breakpoint
/// marker rather than by semicolons.
///
/// The previous implementation split a migration on `;`, which cut a statement
/// in half as soon as a string literal contained one. The first half of this
/// test is that a literal like that survives, the second that a later failure
/// undoes everything before it.
#[tokio::test]
async fn splits_on_breakpoints_and_rolls_back_on_failure() {
    let context = common::context("toasty_migration_atomic");
    let driver = Mssql::new(context.clone());
    driver.reset_db().await.expect("reset_db must succeed");

    let mut conn = driver
        .connect(&ConnectContext::default())
        .await
        .expect("connect must succeed");

    let hints = RenameHints::default();
    let created = driver.generate_migration(&diff(&empty(), &schema_v1(), &hints));
    conn.apply_migration(1, "create items", &created)
        .await
        .expect("the create must apply");

    // A semicolon and a comment marker inside string literals: both used to be
    // treated as structure rather than data.
    let seeds = Migration::new_sql_with_breakpoints(&[
        "INSERT INTO [items] ([id], [name]) VALUES (1, N'semi; colon')",
        "INSERT INTO [items] ([id], [name]) VALUES (2, N'-- not a comment')",
    ]);
    conn.apply_migration(2, "seed", &seeds)
        .await
        .expect("a literal semicolon must not split the statement");

    let rejected = Migration::new_sql_with_breakpoints(&[
        "INSERT INTO [items] ([id], [name]) VALUES (3, N'rolled back')",
        "THIS IS NOT SQL",
    ]);
    let error = conn
        .apply_migration(3, "broken", &rejected)
        .await
        .expect_err("the second statement is not SQL");
    assert!(
        error.to_string().contains("THIS IS NOT SQL")
            || error.to_string().contains("Incorrect syntax"),
        "got: {error}"
    );

    let history: Vec<u64> = conn
        .applied_migrations()
        .await
        .expect("history must be readable")
        .iter()
        .map(|migration| migration.id())
        .collect();
    assert_eq!(
        history,
        vec![1, 2],
        "a failed migration must not be recorded as applied"
    );

    // The statement before the failure must have been rolled back with it.
    Mssql::new(context)
        .execute_raw(
            "IF EXISTS (SELECT 1 FROM [items] WHERE [name] = N'rolled back')
                 THROW 50000, 'the failed migration left its first statement behind', 1;
             IF (SELECT COUNT(*) FROM [items]) <> 2
                 THROW 50000, 'the seed rows are not both there', 1;",
        )
        .await
        .expect("the rows must be exactly the two that were seeded");
}
