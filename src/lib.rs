//! Toasty driver for [Microsoft SQL Server](https://www.microsoft.com/sql-server),
//! speaking the TDS protocol directly through Microsoft's
//! [`mssql-tds`](https://github.com/microsoft/mssql-rs) client.
//!
//! This is a **proof of concept**. Toasty has no SQL Server dialect, so rather
//! than reusing `toasty_sql` this driver renders T-SQL itself from the public
//! [`toasty_core::stmt`] AST and describes its capabilities through a
//! [`Capability`](toasty_core::driver::Capability) that names an existing dialect
//! as a query-planner hint. See [`capability`].
//!
//! There is no `mssql` scheme in `toasty::Db::builder().connect()`, so construct
//! the driver from an `mssql://` URL and hand it to `build()`. The URL form is
//! described by [`MssqlConnectOptions`], which is the same configuration as a
//! value:
//!
//! ```text
//! let driver = toasty_driver_mssql::Mssql::from_url(
//!     "mssql://sa:Password1!@localhost:1433/testdb?encrypt=on&trust_certificate=true",
//! )?;
//!
//! let db = toasty::Db::builder()
//!     .models(toasty::models!(User))
//!     .build(driver)
//!     .await?;
//! ```
//!
//! [`Mssql::new`] takes `mssql-tds`'s own connection configuration instead,
//! re-exported here as [`ClientContext`], so every option that client offers is
//! reachable through this driver without going through a URL:
//!
//! ```text
//! let mut context = toasty_driver_mssql::ClientContext::with_data_source("tcp:localhost,1433");
//! context.user_name = "sa".to_owned();
//! context.password = "Password1!".to_owned();
//! context.database = "testdb".to_owned();
//!
//! let driver = toasty_driver_mssql::Mssql::new(context);
//! ```
//!
//! # Features
//!
//! The default build has none of these; the driver speaks `toasty_core` alone.
//! Each is model-facing, so each needs the `toasty` crate.
//!
//! * `spatial` — `geometry` and `geography` columns, and the methods that
//!   operate on them.
//! * `datetimeoffset` — [`MssqlDateTimeOffset`], which stores an instant with
//!   the offset it is displayed in a `datetimeoffset` column.
//! * `funcs-json`, `funcs-string`, `funcs-math`, `funcs-date` — SQL Server's
//!   built-in functions as extension traits, one Cargo feature per Microsoft
//!   [category][cats]; `funcs` turns on all four. The `funcs` module documents
//!   how a call is carried through an AST that has no node for one.
//!
//! [cats]: https://learn.microsoft.com/en-us/sql/t-sql/functions/functions

#![warn(missing_docs)]

pub mod capability;

mod connection;
#[cfg(feature = "datetimeoffset")]
pub mod datetime_offset;
mod driver;
mod funcs;
mod migration;
pub mod options;
#[cfg(feature = "spatial")]
pub mod spatial;
mod sql;
mod tds;
mod type_map;

/// The `mssql-tds` client this driver speaks TDS with.
///
/// Re-exported because [`Mssql::new`] takes one of its types, and a caller
/// cannot otherwise name that type: the crate is a git dependency pinned to a
/// revision, so a caller who added it separately would get a different type
/// even if the revision matched.
pub use mssql_tds;
/// The connection configuration [`Mssql::new`] takes.
pub use mssql_tds::connection::client_context::ClientContext;
/// The encryption settings inside a [`ClientContext`], and the modes they take.
pub use mssql_tds::core::{EncryptionOptions, EncryptionSetting};

#[cfg(feature = "datetimeoffset")]
pub use datetime_offset::MssqlDateTimeOffset;
pub use driver::Mssql;
pub use options::MssqlConnectOptions;
#[cfg(feature = "funcs-date")]
pub use funcs::DatePart;
#[cfg(feature = "funcs-date")]
pub use funcs::MssqlDate;
#[cfg(feature = "funcs-json")]
pub use funcs::MssqlJson;
#[cfg(feature = "funcs-math")]
pub use funcs::MssqlMath;
#[cfg(feature = "funcs-string")]
pub use funcs::MssqlStr;
#[cfg(feature = "toasty")]
pub use funcs::{MssqlLiteral, lit};
#[cfg(feature = "spatial")]
pub use spatial::{
    MssqlGeography, MssqlGeographyExt, MssqlGeometry, MssqlGeometryExt, MssqlSpatial,
};
