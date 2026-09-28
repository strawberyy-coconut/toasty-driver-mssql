//! The [`Driver`] implementation: connection creation, and the schema-level
//! operations.

use std::borrow::Cow;

use async_trait::async_trait;
use mssql_tds::connection::client_context::ClientContext;
use toasty_core::{
    Error, Result,
    driver::{Capability, ConnectContext, Driver},
    schema::{db::Migration, diff},
};

use crate::{capability, connection::Connection, migration, tds};

/// A SQL Server [`Driver`] that speaks TDS through `mssql-tds`.
///
/// It is built from `mssql-tds`'s own connection configuration, so every option
/// that crate offers is reachable without this driver restating any of them:
///
/// ```no_run
/// use toasty_driver_mssql::{ClientContext, EncryptionOptions, EncryptionSetting, Mssql};
///
/// let mut context = ClientContext::with_data_source("tcp:localhost,1433");
/// context.user_name = "sa".to_owned();
/// context.password = "Password1!".to_owned();
/// context.database = "mydb".to_owned();
/// context.encryption_options = EncryptionOptions {
///     // `ClientContext`'s own default is `Strict`, which is TDS 8.0.
///     mode: EncryptionSetting::On,
///     trust_server_certificate: true,
///     ..Default::default()
/// };
///
/// let driver = Mssql::new(context);
/// ```
pub struct Mssql {
    /// Reused for every connection the driver opens, so one set of credentials
    /// and one set of options serves the whole pool.
    context: ClientContext,
}

impl std::fmt::Debug for Mssql {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `ClientContext` has no `Debug` of its own, and it holds the password,
        // so the fields worth seeing are named one at a time.
        f.debug_struct("Mssql")
            .field("data_source", &self.context.data_source)
            .field("database", &self.context.database)
            .field("user_name", &self.context.user_name)
            .finish_non_exhaustive()
    }
}

impl Mssql {
    /// Creates a driver from a `mssql-tds` connection configuration.
    ///
    /// Nothing is validated here: the configuration is handed to `mssql-tds`
    /// when a connection is opened, which is where it can be reported properly.
    pub fn new(context: ClientContext) -> Self {
        Self { context }
    }

    /// Connects to a different database on the same server.
    async fn connect_database(&self, database: &str) -> Result<tds::Client> {
        let mut context = self.context.clone();
        context.database = database.to_owned();

        tds::Client::connect(&context).await
    }

    /// Runs a single statement that takes no parameters, discarding its result.
    ///
    /// This is the escape hatch for DDL that runs outside the query engine —
    /// the integration suite's per-test table cleanup, for example.
    pub async fn execute_raw(&self, sql: &str) -> Result<()> {
        let mut client = tds::Client::connect(&self.context).await?;
        client.exec(sql.to_owned(), Vec::new(), false).await?;
        Ok(())
    }
}

#[async_trait]
impl Driver for Mssql {
    fn url(&self) -> Cow<'_, str> {
        // The driver need never have seen a URL: what it holds is `mssql-tds`'s
        // data source (`tcp:host,1433`), which is the closest thing to one, and
        // the same answer however the driver was built.
        Cow::Borrowed(&self.context.data_source)
    }

    fn capability(&self) -> &'static Capability {
        &capability::MSSQL
    }

    async fn connect(
        &self,
        _cx: &ConnectContext,
    ) -> Result<Box<dyn toasty_core::driver::Connection>> {
        let client = tds::Client::connect(&self.context).await?;
        Ok(Box::new(Connection::new(client)))
    }

    fn generate_migration(&self, diff: &diff::Schema<'_>) -> Migration {
        // The trait cannot report an error, and a migration that quietly did
        // nothing would still be recorded as applied, so an unrenderable diff
        // becomes SQL that raises when it runs.
        match migration::render_diff(diff) {
            Ok(statements) => Migration::new_sql_with_breakpoints(statements.as_slice()),
            Err(error) => Migration::new_sql(migration::unrenderable(&error)),
        }
    }

    async fn reset_db(&self) -> Result<()> {
        // A URL always names a database, but a context assembled by hand need
        // not — and `DROP DATABASE []` would be a syntax error rather than an
        // answer.
        let database = &self.context.database;

        if database.is_empty() {
            return Err(Error::invalid_driver_configuration(
                "cannot reset a database when the connection configuration names none",
            ));
        }

        let name = database.replace(']', "]]");

        // DROP DATABASE cannot run while the caller is connected to the
        // database, and CREATE DATABASE must be the only statement in its
        // batch, so both go through `master` as dynamic SQL.
        let mut master = self.connect_database("master").await?;

        master
            .exec(
                format!(
                    "IF DB_ID(N'{name}') IS NOT NULL BEGIN \
                     ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
                     DROP DATABASE [{name}]; \
                     END"
                ),
                Vec::new(),
                false,
            )
            .await?;

        master
            .exec(
                format!("EXEC(N'CREATE DATABASE [{name}]')"),
                Vec::new(),
                false,
            )
            .await?;

        Ok(())
    }
}
