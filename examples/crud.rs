//! The basic loop: define a model, create its schema, then insert, query,
//! update and delete rows.
//!
//! ```text
//! docker compose -f compose.dev.yaml up -d db
//! cargo run --example crud
//! ```
//!
//! The example's database is dropped and recreated on every run, so it is safe
//! to repeat.

mod common;

#[derive(Debug, toasty::Model)]
struct Post {
    #[key]
    #[auto]
    id: i64,

    title: String,
    views: i64,
}

#[tokio::main]
async fn main() -> toasty::Result<()> {
    let driver = common::driver("toasty_example_crud").await;

    // There is no `mssql` scheme for `Db::connect`, so the driver is built by
    // hand and handed to `build()`.
    let mut db = toasty::Db::builder()
        .models(toasty::models!(Post))
        .build(driver)
        .await?;

    db.push_schema().await?;

    // `#[auto]` is `IDENTITY(1,1)`, and the generated key comes back from
    // `OUTPUT INSERTED.[id]` in the same round trip — no second query.
    let mut post = toasty::create!(Post {
        title: "T-SQL from a closed AST",
        views: 0,
    })
    .exec(&mut db)
    .await?;

    println!("inserted {:?} as id {}", post.title, post.id);

    // A batch insert is one `INSERT` with several value rows.
    toasty::create!(Post::[
        { title: "Reading the wire", views: 12 },
        { title: "MERGE for upserts", views: 3 },
    ])
    .exec(&mut db)
    .await?;

    // `WHERE` is an expression built from the model's own fields.
    let popular = titles(
        Post::all()
            .filter(Post::fields().views().gt(10))
            .exec(&mut db)
            .await?,
    );
    println!("more than 10 views: {popular:?}");

    // `ORDER BY`, plus `OFFSET … FETCH NEXT` — T-SQL has no `LIMIT`.
    let busiest = titles(
        Post::all()
            .order_by(Post::fields().views().desc())
            .limit(2)
            .exec(&mut db)
            .await?,
    );
    println!("busiest two: {busiest:?}");

    // Aggregates are statements too, so this is a `SELECT COUNT(*)`.
    let total: u64 = Post::all().count().exec(&mut db).await?;
    println!("{total} posts in total");

    // An update through an instance, which the same call writes back.
    toasty::update!(post { views: 7 }).exec(&mut db).await?;
    println!("{:?} now has {} views", post.title, post.views);

    // A delete is a statement of its own, not a query with something appended
    // to it. It answers nothing, so the count afterwards is what shows it ran.
    Post::all()
        .filter(Post::fields().title().eq("MERGE for upserts"))
        .delete()
        .exec(&mut db)
        .await?;

    let remaining: u64 = Post::all().count().exec(&mut db).await?;
    println!("{remaining} posts left after deleting one");

    // A transaction is a scope over the same executor. Nothing inside it is
    // visible outside until `commit`, and `rollback` undoes all of it.
    let mut tx = db.transaction().await?;

    toasty::create!(Post {
        title: "Never committed",
        views: 0,
    })
    .exec(&mut tx)
    .await?;

    let inside: u64 = Post::all().count().exec(&mut tx).await?;
    tx.rollback().await?;

    let after: u64 = Post::all().count().exec(&mut db).await?;

    println!("{inside} inside the transaction, {after} after rolling it back");

    Ok(())
}

/// The titles of a batch of rows, in order.
fn titles(posts: Vec<Post>) -> Vec<String> {
    posts.into_iter().map(|post| post.title).collect()
}
