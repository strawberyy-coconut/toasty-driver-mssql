//! SQL Server `datetimeoffset` columns, against a live server.
//!
//! The codec is covered by unit tests, but they cannot show that the *column*
//! works: whether the server accepts what this driver binds, whether the offset
//! survives the round trip, and whether ordering the column is chronological
//! rather than lexical. The last is the reason the type exists at all — a zoned
//! value stored as text orders by its characters, and its local time precedes
//! another instant's text while following it in time.

use jiff::{Timestamp, tz::Offset};
use toasty::db::Driver as _;
use toasty_driver_mssql::{ClientContext, Mssql, MssqlDateTimeOffset};

mod common;

#[derive(Debug, toasty::Model)]
struct Event {
    #[key]
    id: i64,

    happened_at: MssqlDateTimeOffset,
}

async fn setup(database: &str) -> (toasty::Db, ClientContext) {
    let context = common::context(database);
    let driver = Mssql::new(context.clone());
    driver.reset_db().await.expect("reset_db must succeed");

    let db = toasty::Db::builder()
        .models(toasty::models!(Event))
        .build(driver)
        .await
        .expect("build must succeed");

    db.push_schema().await.expect("push_schema must succeed");

    (db, context)
}

/// Fails the test unless the server agrees with `condition`.
///
/// `execute_raw` discards rows, so the check is stated as a condition the server
/// has to agree with. A misspelled one fails the test rather than passing
/// vacuously.
async fn assert_server(context: &ClientContext, condition: &str, message: &str) {
    let sql = format!("IF NOT ({condition}) THROW 50000, N'{message}', 1;");

    Mssql::new(context.clone())
        .execute_raw(&sql)
        .await
        .unwrap_or_else(|error| panic!("{message}: {error}"));
}

/// The wrapper owns the column type, so a model that names the type never
/// mentions `datetimeoffset`.
#[tokio::test]
async fn the_column_is_a_datetimeoffset() {
    let (_db, context) = setup("toasty_datetimeoffset_column").await;

    assert_server(
        &context,
        "EXISTS (SELECT 1 FROM INFORMATION_SCHEMA.COLUMNS \
         WHERE TABLE_NAME = 'events' AND COLUMN_NAME = 'happened_at' \
         AND DATA_TYPE = 'datetimeoffset')",
        "the column should be datetimeoffset",
    )
    .await;
}

/// The instant and the offset both survive, and the zone comes back as the
/// fixed offset the column carries rather than the name it was written under.
#[tokio::test]
async fn the_offset_survives_the_round_trip() {
    let (mut db, _context) = setup("toasty_datetimeoffset_round_trip").await;

    let instant: Timestamp = "2021-06-15T18:30:00Z".parse().unwrap();
    let offset = Offset::from_seconds(-4 * 3600).unwrap();

    Event::create()
        .id(1)
        .happened_at(MssqlDateTimeOffset::from_timestamp(instant, offset))
        .exec(&mut db)
        .await
        .expect("insert must succeed");

    let read = Event::get_by_id(&mut db, &1)
        .await
        .expect("read must succeed");

    assert_eq!(read.happened_at.timestamp(), instant);
    assert_eq!(read.happened_at.offset(), offset);

    // A `datetimeoffset` has no field for an IANA name, so the zone is the
    // offset itself. `2021-06-15T14:30:00-04:00[-04:00]`.
    let rendered = read.happened_at.to_zoned().to_string();
    assert!(
        rendered.ends_with("[-04:00]"),
        "the zone should be the fixed offset, got: {rendered}"
    );
}

/// The payoff over text storage: `ORDER BY` on the column is chronological even
/// though the displayed local times sort the other way.
#[tokio::test]
async fn ordering_the_column_is_chronological() {
    let (mut db, context) = setup("toasty_datetimeoffset_ordering").await;

    // `09:00+09:00` is `00:00Z`; `00:30-04:00` is `04:30Z`. The later instant
    // has the earlier wall clock, so sorted as text it would come first.
    let earlier: Timestamp = "2021-06-15T00:00:00Z".parse().unwrap();
    let later: Timestamp = "2021-06-15T04:30:00Z".parse().unwrap();

    let tokyo = Offset::from_seconds(9 * 3600).unwrap();
    let new_york = Offset::from_seconds(-4 * 3600).unwrap();

    Event::create()
        .id(1)
        .happened_at(MssqlDateTimeOffset::from_timestamp(earlier, tokyo))
        .exec(&mut db)
        .await
        .expect("insert must succeed");

    Event::create()
        .id(2)
        .happened_at(MssqlDateTimeOffset::from_timestamp(later, new_york))
        .exec(&mut db)
        .await
        .expect("insert must succeed");

    assert_server(
        &context,
        "(SELECT TOP 1 [id] FROM [events] ORDER BY [happened_at] ASC) = 1",
        "the earlier instant should order first",
    )
    .await;

    // The text form the driver uses for a `Zoned` column — the RFC 9557 form
    // with its `[IANA]` annotation — sorts the other way round, which is the
    // ordering the column type exists to avoid.
    let as_text = [
        MssqlDateTimeOffset::from_timestamp(earlier, tokyo)
            .to_zoned()
            .to_string(),
        MssqlDateTimeOffset::from_timestamp(later, new_york)
            .to_zoned()
            .to_string(),
    ];

    assert!(
        as_text[1] < as_text[0],
        "the text form should sort the later instant first: {as_text:?}"
    );
}

/// A `jiff::Timestamp` field can name the column directly with the escape hatch,
/// because a quoted type name is a `db::Type::Custom` and the macro's
/// compatibility check does not apply.
///
/// The field keeps only the instant — the offset is always `+00:00` — but the
/// column is a real `datetimeoffset`. Reading it back has to narrow the zoned
/// value the column returns to the instant the field holds; without that bridge
/// the field is write-only, which is what this pins.
#[tokio::test]
async fn a_timestamp_field_can_name_the_column_directly() {
    #[derive(Debug, toasty::Model)]
    struct Ingest {
        #[key]
        id: i64,

        #[column(type = "DATETIMEOFFSET")]
        inserted_at: Timestamp,
    }

    let context = common::context("toasty_datetimeoffset_timestamp");
    let driver = Mssql::new(context.clone());
    driver.reset_db().await.expect("reset_db must succeed");

    let mut db = toasty::Db::builder()
        .models(toasty::models!(Ingest))
        .build(driver)
        .await
        .expect("build must succeed");

    db.push_schema().await.expect("push_schema must succeed");

    assert_server(
        &context,
        "EXISTS (SELECT 1 FROM INFORMATION_SCHEMA.COLUMNS \
         WHERE TABLE_NAME = 'ingests' AND COLUMN_NAME = 'inserted_at' \
         AND DATA_TYPE = 'datetimeoffset')",
        "the column should be datetimeoffset",
    )
    .await;

    let instant: Timestamp = "2021-06-15T18:30:00Z".parse().unwrap();

    Ingest::create()
        .id(1)
        .inserted_at(instant)
        .exec(&mut db)
        .await
        .expect("insert must succeed");

    let read = Ingest::get_by_id(&mut db, &1)
        .await
        .expect("read must succeed");

    assert_eq!(read.inserted_at, instant);
}
