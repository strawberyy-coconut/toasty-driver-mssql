//! Setup shared by the examples.
//!
//! Each example uses its own database, because `reset_db` drops and recreates
//! the one it is pointed at — two examples sharing a database, or an example
//! and a test run at the same time, would delete each other's schema.

use toasty::db::Driver as _;
use toasty_driver_mssql::{ClientContext, EncryptionOptions, EncryptionSetting, Mssql};

/// The driver for `database`, with that database dropped and recreated.
///
/// The server and the credentials come from the environment as separate values,
/// which is the shape `mssql-tds` takes and what a `ClientContext` is built
/// from. `.dev.env` supplies them to the development container, so inside it the
/// examples need no configuration. From the host, point the data source at the
/// published port instead:
///
/// ```text
/// MSSQL_DATA_SOURCE=tcp:localhost,1434 cargo run --example crud
/// ```
///
/// A `ClientContext` can equally be assembled by hand, which is what to do when
/// the connection is not a set of environment variables:
///
/// ```text
/// let mut context = ClientContext::with_data_source("tcp:localhost,1433");
/// context.user_name = "sa".to_owned();
/// context.password = "Password1!".to_owned();
/// context.database = "testdb".to_owned();
///
/// let driver = Mssql::new(context);
/// ```
pub async fn driver(database: &str) -> Mssql {
    let driver = Mssql::new(context(database));

    driver.reset_db().await.expect("reset_db must succeed");

    driver
}

/// The `mssql-tds` configuration for `database` on the example server.
fn context(database: &str) -> ClientContext {
    let mut context = ClientContext::with_data_source(&env("MSSQL_DATA_SOURCE", "tcp:db,1433"));

    context.user_name = env("MSSQL_USER", "sa");
    context.password = env("MSSQL_PASSWORD", "Password1!");
    context.database = database.to_owned();
    context.encryption_options = EncryptionOptions {
        // `ClientContext`'s own default is `Strict`, which is TDS 8.0 — and SQL
        // Server 2022 does not speak it.
        mode: EncryptionSetting::On,
        // The development container's certificate is self-signed.
        trust_server_certificate: true,
        ..Default::default()
    };

    context
}

/// An environment variable, or `default` when it is unset.
fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}
