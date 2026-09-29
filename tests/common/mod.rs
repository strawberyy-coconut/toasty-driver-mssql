//! Helpers shared by the integration tests.
//!
//! Integration tests are separate binaries, so this is a module each of them
//! includes with `mod common;` rather than a test target of its own.

use toasty_driver_mssql::{ClientContext, MssqlConnectOptions};

/// The development server, as the URL the tests connect with unless
/// `DATABASE_URL` says otherwise.
const DATABASE_URL: &str =
    "mssql://sa:Password1!@db:1433/testdb?encrypt=on&trust_certificate=true";

/// The `mssql-tds` configuration for `database` on the test server.
///
/// Each test gets its own database: `reset_db` drops and recreates the database
/// it is pointed at, so sharing one would make parallel tests delete each
/// other's schema. The URL names a database too, so it is overridden rather
/// than reused.
///
/// The server, credentials and encryption come from `DATABASE_URL`, which
/// `.dev.env` supplies to the development container.
///
/// `encrypt=on&trust_certificate=true` is required rather than decorative: the
/// client's default mode is `Strict`, which is TDS 8.0, and SQL Server 2022
/// does not speak it. The development container's certificate is self-signed.
pub fn context(database: &str) -> ClientContext {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DATABASE_URL.to_owned());

    MssqlConnectOptions::parse(&url)
        .expect("DATABASE_URL must be a valid mssql:// URL")
        .with_database(database)
        .to_client_context()
}
