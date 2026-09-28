//! SQL rendering for the SQL Server driver.
//!
//! Toasty serializes statements through `toasty_sql`, which only speaks the
//! dialects named by [`Dialect`](toasty_core::driver::Dialect). SQL Server is
//! not one of them, so this module renders the public
//! [`toasty_core::stmt`] AST as T-SQL instead.

pub(crate) mod ddl;
pub(crate) mod render;

pub(crate) use render::render;
