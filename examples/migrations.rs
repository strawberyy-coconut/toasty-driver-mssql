//! Migrations, and the `toasty-cli` commands that generate and apply them.
//!
//! ```text
//! docker compose -f compose.dev.yaml up -d db
//! cargo run --example migrations
//! ```
//!
//! Toasty has two ways to get a schema onto a server. `push_schema`, which
//! `examples/crud.rs` uses, creates every table and index from the models each
//! time it runs and forgets what it did. Migrations are the other one: a schema
//! *diff* is rendered to SQL once, written to a file, reviewed, and then
//! applied — and the driver records what it applied, so applying it again is a
//! no-op.
//!
//! The comparison against the previous snapshot, the file layout, the history,
//! and the `_toasty_migrations` tracking table are all Toasty's. What this
//! driver contributes is `Driver::generate_migration`, which turns the diff into
//! T-SQL. That is why the same `toasty-cli` that ships with the built-in drivers
//! drives this one unchanged — the only difference is how the `Db` is built,
//! since there is no `mssql` URL scheme for `Db::connect`:
//!
//! ```ignore
//! let driver = Mssql::from_url(
//!     "mssql://sa:Password1!@localhost:1433/mydb?encrypt=on&trust_certificate=true",
//! )?;
//!
//! let db = toasty::Db::builder()
//!     .models(toasty::models!(crate::*))
//!     .build(driver)
//!     .await?;
//!
//! let cli = ToastyCli::with_config(db, config);
//! cli.parse_and_run().await?;
//! ```
//!
//! A real project puts that in a `src/bin/cli.rs` and runs it with
//! `cargo run --bin cli -- migration generate`. This example is the same thing
//! in one file, so it can do both jobs: run it with arguments and it forwards
//! them to the CLI (`cargo run --example migrations -- migration snapshot`); run
//! it bare and it walks through the workflow below.
//!
//! The migration files are written under `target/`, which is gitignored and
//! wiped before the walkthrough, so every bare run tells the same story from
//! nothing. A real project keeps them beside the code, where they are reviewed
//! and committed alongside the change that prompted them.

mod common;

use std::{fs, path::Path};

use toasty::{db::ConnectContext, migration::History};
use toasty_cli::{Config, MigrationConfig, ToastyCli};

/// The model the migration is generated from.
///
/// `#[auto]` makes the key `IDENTITY(1,1)` and `#[unique]` adds an index, so the
/// generated migration contains both a `CREATE TABLE` and a `CREATE UNIQUE
/// INDEX`.
#[derive(Debug, toasty::Model)]
struct Post {
    #[key]
    #[auto]
    id: i64,

    #[unique]
    slug: String,

    title: String,

    views: i64,
}

/// The database this example drops and recreates, as the other examples do.
const DATABASE: &str = "toasty_example_migrations";

/// Where the generated migration, snapshot and history live.
const MIGRATIONS: &str = "target/migrations-example";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A clean database, so the migration below is what creates the schema.
    let driver = common::driver(DATABASE).await;

    // The one difference from the built-in drivers: the `Db` is handed a driver
    // this crate built, because `Db::connect` has no `mssql` scheme.
    let mut db = toasty::Db::builder()
        .models(toasty::models!(Post))
        .build(driver)
        .await?;

    // `Config` is what a real project writes in `Toasty.toml`; building it in
    // code keeps the example to one file. `statement_breakpoints` keeps its
    // default of `true`, which is what lets this driver split a multi-statement
    // migration into the batches SQL Server will accept.
    let config = Config::new().migration(MigrationConfig::new().path(MIGRATIONS));

    // `ToastyCli` owns a `Db`; ours is cheap to clone (it shares the pool), so
    // keep a handle for the queries below.
    let cli = ToastyCli::with_config(db.clone(), config.clone());

    // With arguments, act as the CLI binary itself. Any migration files from a
    // previous run are left in place, as they would be in a real project.
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        return cli.parse_from(args).await;
    }

    // Bare run: start the walkthrough from an empty scratch directory.
    let _ = fs::remove_dir_all(MIGRATIONS);

    // 1. The schema Toasty derives from the model, printed as TOML. This does
    //    not touch the database or any files.
    cli.parse_from(["toasty", "migration", "snapshot"]).await?;

    // 2. Diff the schema against the previous snapshot — there is none — and
    //    write the SQL, a new snapshot, and a history entry.
    cli.parse_from(["toasty", "migration", "generate", "--name", "initial"])
        .await?;

    // 3. The SQL the driver rendered from the diff. In a real project this is
    //    the file that gets reviewed before it is applied.
    let sql_path = Path::new(MIGRATIONS).join("migrations/0000_initial.sql");
    println!(
        "--- {}\n{}",
        sql_path.display(),
        fs::read_to_string(&sql_path)?
    );

    // 4. Run the pending migration. It is applied in a transaction and recorded
    //    under its id, so a second `apply` would have nothing left to do.
    cli.parse_from(["toasty", "migration", "apply"]).await?;

    // 5. The tables are really there, so the queries the rest of Toasty builds
    //    work against them.
    let post = toasty::create!(Post {
        slug: "tds-direct",
        title: "Talking TDS without ODBC",
        views: 0,
    })
    .exec(&mut db)
    .await?;
    println!("inserted {:?} as id {}", post.title, post.id);

    // 6. Nothing about the model changed, so there is nothing to migrate.
    cli.parse_from(["toasty", "migration", "generate"]).await?;

    // 7. The two halves of the bookkeeping: the history file Toasty wrote, and
    //    the tracking table the driver read back through the same API the CLI
    //    uses. On a fresh run the ids match.
    println!("--- migration history");
    let history = History::load_or_default(&config.migration.get_history_file_path())?;
    for entry in history.entries() {
        println!("{} — {}", entry.id, entry.name);
    }

    println!("--- applied to the database");
    let mut conn = db.driver().connect(&ConnectContext::default()).await?;
    for applied in conn.applied_migrations().await? {
        println!("{}", applied.id());
    }

    Ok(())
}
