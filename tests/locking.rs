//! What the row-lock hint actually does, demonstrated with two connections.
//!
//! The renderer puts `WITH (UPDLOCK, ROWLOCK)` on the `FROM` of a probe that
//! carries `stmt::Lock::Update`, because Toasty's read-modify-write path probes
//! a row and then writes it, and the probe has to keep the row from changing in
//! between. `src/sql/render.rs`'s unit tests pin the *shape* of that hint; this
//! file is where the shape is shown to mean what the renderer says it means.
//!
//! Two things are established, and the second is what makes the first mean
//! anything:
//!
//! * a read taken with the hint still holds its update lock after it returns, so
//!   a concurrent writer blocks until the transaction ends — and when the writer
//!   gives up waiting, the row is left exactly as the reader saw it;
//! * the same read *without* the hint holds nothing, so the writer proceeds. The
//!   hint is therefore the cause, not the transaction or the timing.
//!
//! The SQL below spells the fragment the way `renders_a_row_lock_as_a_table_hint`
//! asserts the renderer spells it, including the `tbl_0_0` alias. The driver has
//! no API that runs a single plan step inside an open transaction, so the
//! fragment is repeated here rather than produced; the two tests have to be kept
//! in step, and the comment on each names the other.

use toasty::db::Driver as _;
use toasty_driver_mssql::{ClientContext, Mssql};

mod common;

/// How long the reader holds its lock and its transaction open. Comfortably
/// longer than the writer's timeout below, so the writer's failure is decided by
/// the lock rather than by a race with the reader finishing.
const HOLD: &str = "00:00:03";

/// How long the writer waits for a lock before giving up. SQL Server reports
/// this as error 1222.
const WRITER_TIMEOUT_MS: u32 = 1000;

async fn setup(database: &str) -> ClientContext {
    let context = common::context(database);
    let driver = Mssql::new(context.clone());
    driver.reset_db().await.expect("reset_db must succeed");

    driver
        .execute_raw(
            "CREATE TABLE [counter] ([id] BIGINT NOT NULL PRIMARY KEY, [value] INT NOT NULL)",
        )
        .await
        .expect("create must succeed");
    driver
        .execute_raw("INSERT INTO [counter] ([id], [value]) VALUES (1, 10)")
        .await
        .expect("insert must succeed");

    context
}

/// Reads `[value]` for `[id] = 1` inside an open transaction that outlives the
/// read, so whatever lock the read took is still held when it returns.
///
/// `hint` is the whole difference between the two tests: an update lock is held
/// to the end of the transaction, while the shared lock a plain read takes under
/// read-committed is released as soon as the statement finishes.
///
/// The fragment is the one `renders_a_row_lock_as_a_table_hint` expects the
/// renderer to emit for a locked probe.
fn hold_a_read(context: ClientContext, hint: &str) -> tokio::task::JoinHandle<()> {
    let sql = format!(
        "BEGIN TRAN; \
         SELECT [value] FROM [counter] AS tbl_0_0{hint} WHERE tbl_0_0.[id] = 1; \
         WAITFOR DELAY '{HOLD}'; \
         ROLLBACK;"
    );

    tokio::spawn(async move {
        Mssql::new(context)
            .execute_raw(&sql)
            .await
            .expect("the holding read must succeed");
    })
}

/// Waits until another session holds a granted update lock in this database, so
/// the writer below cannot race the reader's lock acquisition.
///
/// Polling for the lock rather than sleeping makes the test independent of how
/// fast the first connection gets going: the writer only ever attempts its
/// update once the lock it is supposed to meet is already there.
///
/// A granted lock is reported as `request_status = 'GRANT'` — not `'GRANTED'` —
/// which is the trap that made an earlier version of this poll find nothing.
const WAIT_FOR_THE_LOCK: &str = "DECLARE @waited INT = 0; \
     WHILE @waited < 100 AND NOT EXISTS ( \
         SELECT 1 FROM sys.dm_tran_locks \
         WHERE resource_database_id = DB_ID() \
           AND request_mode = 'U' AND request_status = 'GRANT' \
           AND request_session_id <> @@SPID \
     ) BEGIN WAITFOR DELAY '00:00:00.050'; SET @waited += 1; END; \
     IF @waited >= 100 THROW 50000, 'the probe never took an update lock', 1;";

/// Runs an `UPDATE`, giving up after [`WRITER_TIMEOUT_MS`] if it has to wait.
async fn write(context: &ClientContext, wait_for_the_lock: bool) -> toasty_core::Result<()> {
    let wait = if wait_for_the_lock {
        WAIT_FOR_THE_LOCK
    } else {
        ""
    };
    let sql = format!(
        "{wait} SET LOCK_TIMEOUT {WRITER_TIMEOUT_MS}; \
         UPDATE [counter] SET [value] = 999 WHERE [id] = 1;"
    );

    Mssql::new(context.clone()).execute_raw(&sql).await
}

/// Fails the test unless the row holds `expected`.
///
/// `execute_raw` discards rows, so the check is stated as a condition the server
/// has to agree with. A misspelled one fails the test rather than passing
/// vacuously.
async fn assert_value(context: &ClientContext, expected: i64) {
    let sql = format!(
        "IF NOT EXISTS (SELECT 1 FROM [counter] WHERE [id] = 1 AND [value] = {expected}) \
         THROW 50000, N'the row did not hold {expected}', 1;"
    );

    Mssql::new(context.clone())
        .execute_raw(&sql)
        .await
        .unwrap_or_else(|error| panic!("the row did not hold {expected}: {error}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_update_lock_still_blocks_a_writer_after_the_read_returns() {
    let context = setup("toasty_locking").await;

    let reader = hold_a_read(
        context.clone(),
        // Exactly what `renders_a_row_lock_as_a_table_hint` pins the renderer to.
        " WITH (UPDLOCK, ROWLOCK)",
    );

    // The update lock is held, so the writer waits, times out, and fails as a
    // retryable conflict rather than as an opaque driver error.
    let error = write(&context, true)
        .await
        .expect_err("the writer must not be able to take the locked row");

    assert!(
        error.is_serialization_failure(),
        "a lock timeout is a retryable conflict, got: {error:?}"
    );
    // Named explicitly so this cannot pass on a deadlock, which is classified
    // the same way but means something else. The number is not in the message:
    // the classification keeps the server's own text.
    assert!(
        error
            .to_string()
            .to_lowercase()
            .contains("lock request time out"),
        "expected a lock request timeout, got: {error}"
    );

    // And the outcome the lock exists for: the row the reader saw is the row
    // that is still there.
    assert_value(&context, 10).await;

    reader.await.expect("the reader must not panic");
}

/// The negative control. Without the hint the read holds nothing once it
/// returns, so the writer goes straight through — which is what makes the test
/// above evidence for the hint rather than for the transaction or the delay.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_read_holds_nothing_and_the_writer_proceeds() {
    let context = setup("toasty_locking_control").await;

    let reader = hold_a_read(
        context.clone(),
        // The same read, minus the hint.
        "",
    );

    // No `WAIT_FOR_THE_LOCK` here: there is no lock to wait for, which is the
    // point. The writer succeeds whether or not the reader's transaction is
    // still open.
    write(&context, false)
        .await
        .expect("an unheld row must accept the write");

    // The write landed, so the row is no longer what the reader saw.
    assert_value(&context, 999).await;

    reader.await.expect("the reader must not panic");
}
