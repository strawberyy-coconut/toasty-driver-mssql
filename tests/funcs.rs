//! SQL Server scalar functions, against a live server.
//!
//! These are carried through a node that Toasty uses for something else (see
//! `src/funcs.rs`), so the point of this file is to prove the carrier works for
//! real: every function is executed by the server, not merely rendered. A
//! rendering test could only tell us the SQL matched what we wrote down, and if
//! we wrote down `DATEPART(col, day)` instead of `DATEPART(day, col)` that test
//! would pass while the query never could.
//!
//! The reverse is also true, so the file does both:
//! `every_function_is_accepted_by_the_server` runs each function once to prove
//! the server takes the SQL, and the five tests after it check that the
//! functions answer the *right rows*.

use std::str::FromStr as _;

use toasty::db::Driver as _;
use toasty::stmt::{List, Query, Timestamp};
use toasty_driver_mssql::{
    DatePart, Mssql, MssqlDate as _, MssqlJson as _, MssqlMath as _, MssqlStr as _, lit,
};

mod common;

#[derive(Debug, toasty::Model)]
struct Widget {
    #[key]
    id: i64,

    /// JSON text, the storage this driver gives `Vec<scalar>` and `#[document]`.
    notes: String,

    score: i64,
    price: f64,
    created: Timestamp,
}

async fn setup(database: &str) -> toasty::Db {
    let driver = Mssql::new(common::context(database));
    driver.reset_db().await.expect("reset_db must succeed");

    let db = toasty::Db::builder()
        .models(toasty::models!(Widget))
        .build(driver)
        .await
        .expect("build must succeed");

    db.push_schema().await.expect("push_schema must succeed");

    db
}

fn at(text: &str) -> Timestamp {
    Timestamp::from_str(text).expect("a valid timestamp")
}

/// Four rows chosen so each function has something to answer about: an object,
/// an array, junk, and a value with padding.
async fn seed(db: &mut toasty::Db) {
    let rows = [
        (1_i64, "{}", -7_i64, 2.3456_f64, "2024-03-15T10:20:30Z"),
        (2, "[1,2]", 5, 1.5, "2024-12-31T23:59:59Z"),
        (3, "not json", 0, 1.0, "2023-01-01T00:00:00Z"),
        (4, "   spaced   ", 12, 100.0, "2022-06-01T00:00:00Z"),
    ];

    for (id, notes, score, price, created) in rows {
        toasty::create!(Widget {
            id: id,
            notes: notes,
            score: score,
            price: price,
            created: at(created),
        })
        .exec(db)
        .await
        .expect("create");
    }
}

/// Rows matching `filter`, as ids.
async fn ids(db: &mut toasty::Db, filter: toasty::stmt::Expr<bool>) -> Vec<i64> {
    let mut rows: Vec<Widget> = Query::<List<Widget>>::all()
        .filter(filter)
        .exec(db)
        .await
        .expect("query must succeed");

    rows.sort_by_key(|row| row.id);

    rows.into_iter().map(|row| row.id).collect()
}

/// Every function, once, to prove the server accepts the SQL it renders to.
///
/// A value-returning function is compared against something, because a bare
/// call cannot stand in a `WHERE`. The comparison is deliberately trivial; the
/// checks on the *answers* are in the tests below.
#[tokio::test]
async fn every_function_is_accepted_by_the_server() {
    let mut db = setup("toasty_funcs_accepted").await;
    seed(&mut db).await;

    let notes = Widget::fields().notes();
    let score = Widget::fields().score();
    let price = Widget::fields().price();
    let created = Widget::fields().created();

    let cases: Vec<(&str, toasty::stmt::Expr<bool>)> = vec![
        // JSON.
        ("is_json", notes.is_json()),
        ("is_json_array", notes.is_json_array()),
        ("is_json_object", notes.is_json_object()),
        ("is_json_scalar", notes.is_json_scalar()),
        ("is_json_value", notes.is_json_value()),
        ("json_path_exists", notes.json_path_exists("a")),
        // String.
        ("len gt", notes.len().gt(0)),
        ("is_empty", notes.is_empty()),
        ("datalength gt", notes.datalength().gt(0)),
        ("char_index gt", notes.char_index("j").gt(0)),
        ("pat_index gt", notes.pat_index("%j%").gt(0)),
        ("ascii gt", notes.ascii().gt(0)),
        ("unicode gt", notes.unicode().gt(0)),
        ("difference gt", notes.difference("json").gt(0)),
        ("is_date", notes.is_date()),
        ("upper eq", notes.upper().eq("X")),
        ("lower eq", notes.lower().eq("X")),
        ("trim eq", notes.trim().eq("X")),
        ("ltrim eq", notes.ltrim().eq("X")),
        ("rtrim eq", notes.rtrim().eq("X")),
        ("reverse eq", notes.reverse().eq("X")),
        ("soundex eq", notes.soundex().eq("X")),
        ("replace eq", notes.replace("a", "b").eq("X")),
        ("substring eq", notes.substring(1, 2).eq("X")),
        ("left eq", notes.left(2).eq("X")),
        ("right eq", notes.right(2).eq("X")),
        ("replicate eq", notes.replicate(2).eq("X")),
        // Numeric.
        ("abs gt", score.abs().gt(0)),
        ("sign gt", score.sign().gt(-2)),
        ("square gt", score.square().gt(-1)),
        ("ceiling gt", score.ceiling().gt(-100)),
        ("floor gt", score.floor().gt(-100)),
        ("round eq", score.round(1).eq(0)),
        // A float answer has to be compared against an inlined constant: the
        // engine cannot type a parameter opposite a function call. See `lit`.
        ("sqrt gt", price.sqrt().gt(lit(0.0))),
        ("power gt", price.power(2.0).gt(lit(0.0))),
        ("log gt", price.log().gt(lit(-100.0))),
        ("log10 gt", price.log10().gt(lit(-100.0))),
        ("exp gt", price.exp().gt(lit(0.0))),
        // Date and time.
        ("year gt", created.year().gt(0)),
        ("month gt", created.month().gt(0)),
        ("day gt", created.day().gt(0)),
        ("hour gt", created.hour().gt(-1)),
        ("minute gt", created.minute().gt(-1)),
        ("second gt", created.second().gt(-1)),
        ("date_part gt", created.date_part(DatePart::DayOfYear).gt(0)),
        (
            "date_add gt",
            created
                .date_add(DatePart::Day, 1)
                .gt(at("1970-01-01T00:00:00Z")),
        ),
        (
            "date_trunc gt",
            created
                .date_trunc(DatePart::Year)
                .gt(at("1970-01-01T00:00:00Z")),
        ),
        (
            "eomonth gt",
            created.eomonth().gt(at("1970-01-01T00:00:00Z")),
        ),
        (
            "eomonth_offset gt",
            created.eomonth_offset(1).gt(at("1970-01-01T00:00:00Z")),
        ),
    ];

    let mut failures = Vec::new();

    for (name, filter) in cases {
        let result: Result<Vec<Widget>, _> = Query::<List<Widget>>::all()
            .filter(filter)
            .exec(&mut db)
            .await;

        if let Err(error) = result {
            failures.push(format!("{name}: {error}"));
        }
    }

    assert!(
        failures.is_empty(),
        "{} rejected:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The JSON path readers get a database of their own.
///
/// `JSON_VALUE` and `JSON_QUERY` do not return NULL on text that is not JSON —
/// they *raise* (error 13609), which fails the whole query. The mixed bag above
/// contains a row of junk, so running them there would test the seed data rather
/// than the binding. Note this is a real hazard for callers too: a filter mixing
/// `is_json` with `json_value` is not safe, because SQL Server does not promise
/// to evaluate them in that order.
#[tokio::test]
async fn json_path_readers_read_a_valid_document() {
    let mut db = setup("toasty_funcs_json_read").await;

    for (id, notes) in [(1_i64, "{}"), (2, "[1,2]"), (3, "{\"a\":\"x\"}")] {
        toasty::create!(Widget {
            id: id,
            notes: notes,
            score: 1,
            price: 1.0,
            created: at("2024-01-01T00:00:00Z"),
        })
        .exec(&mut db)
        .await
        .expect("create");
    }

    let notes = Widget::fields().notes();

    // `$.a` exists in one row, and holds a scalar the extractor can return.
    assert_eq!(ids(&mut db, notes.json_path_exists("a")).await, [3]);
    assert_eq!(ids(&mut db, notes.json_value("a").eq("x")).await, [3]);

    // A path that is absent is NULL in every row, so it matches nothing — and
    // an absent path is not an error, unlike absent JSON.
    assert_eq!(
        ids(&mut db, notes.json_value("missing").eq("x")).await,
        Vec::<i64>::new()
    );

    // `json_query` is the one to reach for when the answer is an object or
    // array: `json_value` would answer NULL for these.
    assert_eq!(
        ids(&mut db, notes.json_query("a").eq("x")).await,
        Vec::<i64>::new()
    );
}

/// A call can be the receiver of another, so one function's answer can be read
/// by the next.
///
/// This is what the `Expr` half of each trait is for: the methods are declared
/// once with the operand as their only required piece, so implementing it for a
/// path and for an expression makes the whole vocabulary composable.
#[tokio::test]
async fn a_call_can_be_another_calls_receiver() {
    let mut db = setup("toasty_funcs_nested").await;
    seed(&mut db).await;

    let notes = Widget::fields().notes();
    let created = Widget::fields().created();

    // `CHARINDEX` reading what `UPPER` answered.
    assert_eq!(
        ids(&mut db, notes.upper().char_index("JSON").gt(0)).await,
        [3]
    );

    // `YEAR` reading what `DATEADD` answered: 2024 plus a year.
    assert_eq!(
        ids(&mut db, created.date_add(DatePart::Year, 1).year().eq(2025)).await,
        [1, 2]
    );

    // `LEN` reading what `REPLACE` answered: "not json" without "not " is four.
    assert_eq!(
        ids(&mut db, notes.replace("not ", "").len().eq(4)).await,
        [3]
    );
}

/// The JSON checks tell an object from an array from junk.
#[tokio::test]
async fn json_checks_distinguish_shapes() {
    let mut db = setup("toasty_funcs_json").await;
    seed(&mut db).await;

    let notes = Widget::fields().notes();

    assert_eq!(ids(&mut db, notes.is_json()).await, [1, 2]);
    assert_eq!(ids(&mut db, notes.is_json_object()).await, [1]);
    assert_eq!(ids(&mut db, notes.is_json_array()).await, [2]);

    // `VALUE` is the loosest of the four: any JSON at all, including a bare
    // scalar. None of the seeded rows is one, so this can only be the two.
    assert_eq!(ids(&mut db, notes.is_json_value()).await, [1, 2]);

    // The scalar constraint is the strictest, and no seeded row is a bare
    // scalar — so it matches nothing rather than everything.
    assert_eq!(
        ids(&mut db, notes.is_json_scalar()).await,
        Vec::<i64>::new()
    );
}

/// `LEN` is not `DATALENGTH`, and neither is `TRIM`.
#[tokio::test]
async fn string_functions_measure_as_documented() {
    let mut db = setup("toasty_funcs_string").await;
    seed(&mut db).await;

    let notes = Widget::fields().notes();

    // "   spaced   " is twelve characters: LEN drops the trailing padding and
    // keeps the leading, DATALENGTH counts every byte, two per nvarchar
    // character. "not json" is eight, so the two cannot be confused for one
    // another.
    assert_eq!(ids(&mut db, notes.len().eq(9)).await, [4]);
    assert_eq!(ids(&mut db, notes.datalength().eq(24)).await, [4]);
    assert_eq!(ids(&mut db, notes.len().eq(8)).await, [3]);
    assert_eq!(ids(&mut db, notes.trim().eq("spaced")).await, [4]);

    assert_eq!(ids(&mut db, notes.char_index("json").gt(0)).await, [3]);
    assert_eq!(ids(&mut db, notes.upper().eq("NOT JSON")).await, [3]);
    assert_eq!(ids(&mut db, notes.reverse().eq("nosj ton")).await, [3]);
    assert_eq!(
        ids(&mut db, notes.replace("not", "yes").eq("yes json")).await,
        [3]
    );
    assert_eq!(ids(&mut db, notes.substring(1, 3).eq("not")).await, [3]);
    assert_eq!(ids(&mut db, notes.left(3).eq("not")).await, [3]);
    assert_eq!(ids(&mut db, notes.right(4).eq("json")).await, [3]);
}

/// Arithmetic and date functions answer the rows their names promise.
#[tokio::test]
async fn numeric_and_date_functions_answer_the_right_rows() {
    let mut db = setup("toasty_funcs_numeric").await;
    seed(&mut db).await;

    let score = Widget::fields().score();
    let price = Widget::fields().price();
    let created = Widget::fields().created();

    assert_eq!(ids(&mut db, score.abs().eq(7)).await, [1]);
    assert_eq!(ids(&mut db, score.sign().eq(-1)).await, [1]);
    assert_eq!(ids(&mut db, price.floor().eq(lit(2.0))).await, [1]);
    assert_eq!(ids(&mut db, price.ceiling().eq(lit(3.0))).await, [1]);

    assert_eq!(ids(&mut db, created.year().eq(2024)).await, [1, 2]);
    assert_eq!(ids(&mut db, created.month().eq(3)).await, [1]);
    assert_eq!(
        ids(&mut db, created.date_part(DatePart::DayOfYear).eq(75)).await,
        [1]
    );

    // Truncating to the year throws away month, day and time — so the two 2024
    // rows become the same instant.
    assert_eq!(
        ids(
            &mut db,
            created
                .date_trunc(DatePart::Year)
                .eq(at("2024-01-01T00:00:00Z"))
        )
        .await,
        [1, 2]
    );

    assert_eq!(
        ids(&mut db, created.eomonth().eq(at("2024-03-31T00:00:00Z"))).await,
        [1]
    );
}
