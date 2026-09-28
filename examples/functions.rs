//! SQL Server's built-in functions, called from a query filter.
//!
//! ```text
//! docker compose -f compose.dev.yaml up -d db
//! cargo run --example functions --features funcs
//! ```
//!
//! `funcs` turns on all four categories — JSON, string, mathematical and date
//! and time. Each is a separate Cargo feature, so a caller who wants JSON
//! validation is not also handed the date and math vocabulary.

use std::str::FromStr as _;

use toasty::stmt::{List, Query, Timestamp};
use toasty_driver_mssql::{
    DatePart, MssqlDate as _, MssqlJson as _, MssqlMath as _, MssqlStr as _, lit,
};

mod common;

/// A document held as JSON text, a price, and a timestamp: one column per
/// category of function below.
///
/// SQL Server has no native JSON column type, so `notes` is an ordinary
/// `NVARCHAR(MAX)` column — the storage this driver gives a `Vec<scalar>` or
/// `#[document]` field, and the storage SQL Server's own documentation
/// prescribes. That leaves the column with no constraint on it, which is why
/// `ISJSON` earns its keep.
#[derive(Debug, toasty::Model)]
struct Doc {
    #[key]
    id: i64,

    notes: String,
    price: f64,
    created: Timestamp,
}

#[tokio::main]
async fn main() -> toasty::Result<()> {
    let driver = common::driver("toasty_example_functions").await;

    let mut db = toasty::Db::builder()
        .models(toasty::models!(Doc))
        .build(driver)
        .await?;

    db.push_schema().await?;

    for (id, notes, price, created) in [
        (1_i64, r#"{"a": 1}"#, 1.5_f64, "2024-03-15T10:20:30Z"),
        (2, "[1, 2, 3]", 16.0, "2024-12-31T23:59:59Z"),
        (3, "not json at all", 100.0, "2023-01-01T00:00:00Z"),
    ] {
        toasty::create!(Doc {
            id: id,
            notes: notes,
            price: price,
            created: Timestamp::from_str(created).expect("a valid timestamp"),
        })
        .exec(&mut db)
        .await?;
    }

    let doc = Doc::fields();

    // JSON. `ISJSON` is the integrity check an `NVARCHAR(MAX)` JSON column
    // otherwise does not have.
    let objects = matching(&mut db, doc.notes().is_json_object()).await;
    println!("notes that are a JSON object: {objects:?}");

    let arrays = matching(&mut db, doc.notes().is_json_array()).await;
    println!("notes that are a JSON array:  {arrays:?}");

    let scalars = matching(&mut db, doc.notes().is_json_scalar()).await;
    println!("notes that are a JSON scalar: {scalars:?}");

    // Strings. A call answers a value, so it composes with the next one:
    // `upper()` answers text and `char_index()` reads text.
    let shouting = matching(&mut db, doc.notes().upper().char_index("JSON").gt(0)).await;
    println!("notes containing \"json\", ignoring case: {shouting:?}");

    // The comparison sits on the ordinary operators, not on the function.
    let long_notes = matching(&mut db, doc.notes().len().gt(11)).await;
    println!("notes longer than 11 characters: {long_notes:?}");

    // Numbers. A float constant opposite a call has to be wrapped in `lit()`,
    // because the engine cannot infer the parameter's type from a function —
    // see the README's note on `lit`.
    let cheap_roots = matching(&mut db, doc.price().sqrt().lt(lit(4.0))).await;
    println!("prices whose square root is under 4: {cheap_roots:?}");

    let rounded = matching(&mut db, doc.price().round(0).eq(lit(16.0))).await;
    println!("prices that round to 16: {rounded:?}");

    // Dates. A date part travels as a bare keyword, because that is what the
    // grammar takes.
    let created_2024 = matching(
        &mut db,
        doc.created().date_add(DatePart::Year, 0).year().eq(2024),
    )
    .await;
    println!("created in 2024: {created_2024:?}");

    // `EOMONTH` answers the last day of the column's month, and the result is a
    // date again, so it can be read further.
    let in_march = matching(&mut db, doc.created().eomonth().month().eq(3)).await;
    println!("created in March: {in_march:?}");

    let after_midday = matching(&mut db, doc.created().hour().ge(12)).await;
    println!("created at or after midday: {after_midday:?}");

    Ok(())
}

/// The ids of the rows matching `filter`, in order.
async fn matching(db: &mut toasty::Db, filter: toasty::stmt::Expr<bool>) -> Vec<i64> {
    let mut rows: Vec<Doc> = Query::<List<Doc>>::all()
        .filter(filter)
        .exec(db)
        .await
        .expect("the query must succeed");

    rows.sort_by_key(|row| row.id);

    rows.into_iter().map(|row| row.id).collect()
}
