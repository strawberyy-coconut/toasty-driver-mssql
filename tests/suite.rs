//! Runs Toasty's published driver conformance suite against SQL Server.
//!
//! The suite gates each test on a driver capability, so flags this driver
//! reports as `false` make the tests it cannot serve *skip* rather than fail.
//! That list is restated for the macro at the bottom of this file, because the
//! macro cannot read a `&'static Capability` at compile time.
//!
//! Run with `cargo test --test suite`. It needs the SQL Server container from
//! `compose.dev.yaml`.

use std::sync::OnceLock;

use toasty::db::Driver as ToastyDriver;
use toasty_driver_integration_suite::Setup;
use toasty_driver_mssql::Mssql;

mod common;

/// The database the suite runs in, kept separate from the driver's own
/// integration tests so a suite run never disturbs them.
const SUITE_DATABASE: &str = "toasty_suite";

/// The suite's configuration, on the server the other tests use.
fn context() -> toasty_driver_mssql::ClientContext {
    common::context(SUITE_DATABASE)
}

/// Drops and recreates the suite's database, exactly once per test process.
///
/// The suite isolates tests from each other by prefixing table names, so one
/// shared database is enough — but it has to exist before the first
/// `push_schema`, and `Setup::driver()` is synchronous.
///
/// The work therefore runs on a dedicated thread with its own runtime: blocking
/// on I/O from inside a test's runtime would deadlock a current-thread runtime.
fn ensure_database() {
    static INIT: OnceLock<()> = OnceLock::new();

    INIT.get_or_init(|| {
        std::thread::spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime must be constructed")
                .block_on(async {
                    let driver = Mssql::new(context());

                    // `reset_db` drops the database if it exists and recreates
                    // it, which also gives every suite run an empty schema.
                    ToastyDriver::reset_db(&driver)
                        .await
                        .expect("reset_db must succeed");
                });
        })
        .join()
        .expect("the database setup thread must not panic");
    });
}

#[derive(Debug)]
struct MssqlSetup;

impl MssqlSetup {
    fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Setup for MssqlSetup {
    fn driver(&self) -> Box<dyn ToastyDriver> {
        ensure_database();

        Box::new(Mssql::new(context()))
    }

    async fn delete_table(&self, name: &str) {
        let driver = Mssql::new(context());

        driver
            .execute_raw(&format!("DROP TABLE IF EXISTS {}", quote_ident(name)))
            .await
            .expect("dropping a test table must succeed");
    }
}

/// Quotes an identifier for T-SQL.
fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

// Restates the capabilities this driver reports as `false`. The suite defaults
// every flag to `true`, and its `validate_driver_capabilities` test asserts that
// each flag matches the driver's own `Capability`, so an omission here fails
// loudly rather than silently skipping.
//
// The list also decides which tests are *generated*: a test whose `requires`
// names a flag that is `false` here never reaches the binary, so a missing entry
// understates what the driver is asked to do rather than showing up as a
// failure. That is why enabling upserts added nineteen tests at once, and why
// enabling `vec_scalar` added another twenty.
//
// The three `vec_*` removal flags stay `false`: a JSON array has no in-place
// value or index removal in T-SQL, so those operations are rejected rather than
// emulated.
toasty_driver_integration_suite::generate_driver_tests!(
    MssqlSetup::new(),
    bigdecimal_implemented: false,
    decimal_arbitrary_precision: false,
    native_array: false,
    native_cidr: false,
    native_enum: false,
    native_ilike: false,
    native_inet: false,
    native_json: false,
    native_jsonb: false,
    native_macaddr: false,
    native_macaddr8: false,
    scan: false,
    transaction_lock_mode: false,
    unique_list_index: false,
    vec_pop: false,
    vec_remove: false,
    vec_remove_at: false,
);
