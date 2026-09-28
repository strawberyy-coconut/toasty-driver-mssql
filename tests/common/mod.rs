//! Helpers shared by the integration tests.
//!
//! Integration tests are separate binaries, so this is a module each of them
//! includes with `mod common;` rather than a test target of its own.

use toasty_driver_mssql::{ClientContext, EncryptionOptions, EncryptionSetting};

/// The `mssql-tds` configuration for `database` on the test server.
///
/// Each test gets its own database: `reset_db` drops and recreates the database
/// it is pointed at, so sharing one would make parallel tests delete each
/// other's schema.
///
/// The server and the credentials come from the environment as separate values,
/// which is the shape `mssql-tds` takes. `.dev.env` supplies them to the
/// development container.
pub fn context(database: &str) -> ClientContext {
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
