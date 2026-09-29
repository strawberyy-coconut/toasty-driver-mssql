//! The `mssql://` URL path, against a live server.
//!
//! `src/options.rs` covers parsing on its own; what it cannot show is that the
//! options a URL produces actually connect — the data source, the credentials
//! and the encryption mode all have to agree with the server. That is what this
//! test does: nothing but a URL is handed to the driver.
//!
//! Run with `cargo test --test url`. It needs the SQL Server container from
//! `compose.dev.yaml`.

use toasty::db::Driver as _;
use toasty_core::driver::ConnectionUrl;
use toasty_driver_mssql::Mssql;

/// The database this test owns.
///
/// Each test target uses one of its own, because `reset_db` drops and recreates
/// whatever it is pointed at.
const DATABASE: &str = "toasty_url";

/// The development server, used when `DATABASE_URL` is unset.
const DEFAULT_DATABASE_URL: &str =
    "mssql://sa:Password1!@db:1433/testdb?encrypt=on&trust_certificate=true";

#[derive(Debug, toasty::Model)]
struct Note {
    #[key]
    id: i64,
    body: String,
}

/// `DATABASE_URL` pointed at this test's own database.
///
/// The URL names a database the other tests also use, so the path is swapped
/// for one of this test's own before the driver is built — the point being that
/// what reaches the driver is still just a URL.
fn url() -> String {
    let base = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_owned());
    let path = ConnectionUrl::parse(&base)
        .expect("DATABASE_URL must be a valid connection URL")
        .path()
        .to_owned();

    assert!(
        !path.is_empty(),
        "DATABASE_URL must name a database, as in `mssql://sa:pw@localhost:1433/mydb`"
    );

    // The first occurrence of the path is the path itself, so replacing it once
    // swaps the database and leaves the authority and the query string alone.
    base.replacen(&path, &format!("/{DATABASE}"), 1)
}

#[tokio::test]
async fn a_url_is_enough_to_connect_and_run() {
    let url = url();
    let driver = Mssql::from_url(&url).expect("the URL must parse");

    // The URL is what the driver reports, which is what a caller redacting a
    // password for display starts from.
    assert_eq!(driver.url().as_ref(), url.as_str());

    driver.reset_db().await.expect("reset_db must succeed");

    let mut db = toasty::Db::builder()
        .models(toasty::models!(Note))
        .build(driver)
        .await
        .expect("build must succeed");

    db.push_schema().await.expect("push_schema must succeed");

    toasty::create!(Note {
        id: 1,
        body: "written through a URL",
    })
    .exec(&mut db)
    .await
    .expect("the insert must succeed");

    let notes: Vec<Note> = Note::all()
        .exec(&mut db)
        .await
        .expect("the query must succeed");

    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].body, "written through a URL");
}
