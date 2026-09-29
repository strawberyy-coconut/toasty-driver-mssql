//! Setup shared by the examples.
//!
//! Each example uses its own database, because `reset_db` drops and recreates
//! the one it is pointed at — two examples sharing a database, or an example
//! and a test run at the same time, would delete each other's schema.

use toasty::db::Driver as _;
use toasty_driver_mssql::{Mssql, MssqlConnectOptions};

/// The development server, as the URL the examples connect with unless
/// `DATABASE_URL` says otherwise.
const DATABASE_URL: &str =
    "mssql://sa:Password1!@db:1433/testdb?encrypt=on&trust_certificate=true";

/// The driver for `database`, with that database dropped and recreated.
///
/// The connection comes from `DATABASE_URL`, which `.dev.env` supplies to the
/// development container, so inside it the examples need no configuration. From
/// the host, point the URL at the published port instead:
///
/// ```text
/// DATABASE_URL='mssql://sa:Password1!@localhost:1434/testdb?encrypt=on&trust_certificate=true' \
///     cargo run --example crud
/// ```
///
/// Those query parameters are required rather than decorative: the client's
/// default encryption mode is `Strict`, which is TDS 8.0 and which SQL Server
/// 2022 does not speak, and the development container's certificate is
/// self-signed.
///
/// A URL is one way to build the driver. `MssqlConnectOptions` can equally be
/// assembled in code and handed to [`Mssql::from_options`].
pub async fn driver(database: &str) -> Mssql {
    let driver = Mssql::from_options(&options(database));

    driver.reset_db().await.expect("reset_db must succeed");

    driver
}

/// The connection options for `database`, taken from `DATABASE_URL`.
///
/// The URL names a database of its own — the one `reset_db` would drop — so each
/// example overrides it rather than reusing it.
fn options(database: &str) -> MssqlConnectOptions {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DATABASE_URL.to_owned());

    MssqlConnectOptions::parse(&url)
        .expect("DATABASE_URL must be a valid mssql:// URL")
        .with_database(database)
}
