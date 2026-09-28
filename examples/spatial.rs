//! Spatial columns: writing a shape computed in Rust, and asking questions
//! about it with SQL Server's spatial methods.
//!
//! ```text
//! docker compose -f compose.dev.yaml up -d db
//! cargo run --example spatial --features spatial
//! ```
//!
//! SQL Server stores a spatial value in its own serialization (MS-SSCLRT), not
//! WKB, and sends it as `varbinary`. The types in this crate carry that payload
//! and convert to and from `geo_types`.

use geo_types::{Coord, Geometry, LineString, Polygon};
use toasty::stmt::{List, Query};
use toasty_driver_mssql::{
    MssqlGeography, MssqlGeographyExt as _, MssqlGeometry, MssqlGeometryExt as _,
    MssqlSpatial as _, lit,
};

mod common;

/// `plot` is a planar `geometry`; `route` is a round-earth `geography`.
///
/// The two are not interchangeable even though they share a payload format:
/// SQL Server reads the same ring the other way round for `geography`, so
/// identical bytes can be a triangle of half a square unit or the rest of the
/// globe. That is why the type owns the column type rather than a parameter.
#[derive(Debug, toasty::Model)]
struct Region {
    #[key]
    id: i64,

    plot: MssqlGeometry,
    route: MssqlGeography,
}

#[tokio::main]
async fn main() -> toasty::Result<()> {
    let driver = common::driver("toasty_example_spatial").await;

    let mut db = toasty::Db::builder()
        .models(toasty::models!(Region))
        .build(driver)
        .await?;

    db.push_schema().await?;

    // A 4x4 square with a 1x1 hole in it, so the encoder has to write the
    // exterior/interior figure attributes and both rings. The area is 15.
    let square = Polygon::new(
        ring(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0), (0.0, 0.0)]),
        vec![ring(&[
            (1.0, 1.0),
            (2.0, 1.0),
            (2.0, 2.0),
            (1.0, 2.0),
            (1.0, 1.0),
        ])],
    );

    // `geography` reads a polygon's interior by the left-hand rule, so this
    // column gets a line instead — it has no orientation to get wrong.
    let route = LineString::new(vec![Coord { x: 1.0, y: 2.0 }, Coord { x: 3.0, y: 4.0 }]);

    let mut region = toasty::create!(Region {
        id: 1_i64,
        plot: MssqlGeometry::from_geometry(Geometry::Polygon(square), 0),
        route: MssqlGeography::from_geometry(Geometry::LineString(route), 4326),
    })
    .exec(&mut db)
    .await?;

    // Reading the row back gives the payload the server sent, which decodes to
    // the shape that went in.
    println!(
        "plot is a {:?} with SRID {:?}",
        region.plot.to_geometry().expect("must decode"),
        region.plot.srid(),
    );

    let plot = Region::fields().plot();

    // A spatial method is a *method*: T-SQL has no `STArea(col)`, only
    // `col.STArea()`. The carrier this crate uses spells both.
    // The plot is a 4x4 square with a 1x1 hole in it, so its area is 15, and
    // the hole is why the figure attributes above had to be right.
    let full_area = matching(&mut db, plot.st_area().gt(lit(14.9999))).await;
    println!("rows whose plot has an area of about 15: {full_area:?}");

    // Predicates take the other shape as well-known text, wrapped in the
    // constructor for the column's *own* type — `geometry::STGeomFromText` for
    // a `geometry` column, `geography::STGeomFromText` for a `geography` one.
    let hits = matching(&mut db, plot.st_intersects("POINT(0.5 0.5)", 0)).await;
    println!("plots the point (0.5, 0.5) falls in: {hits:?}");

    // A point in the body is one the plot *contains*; `STIntersects` above
    // would be true for a point on the boundary too.
    let contains = matching(&mut db, plot.st_contains("POINT(0.5 0.5)", 0)).await;
    println!("plots that contain it outright: {contains:?}");

    // The same point moved into the hole is within the plot's extent but not in
    // the plot, which is what makes the hole a hole.
    let in_hole = matching(&mut db, plot.st_contains("POINT(1.5 1.5)", 0)).await;
    println!("plots containing the point (1.5, 1.5), which is in the hole: {in_hole:?}");

    // A method that answers a *shape* is only useful chained: a geometry has no
    // `=` operator, so it is the measurement that says anything about it. The
    // union of the 15-unit plot with a 6x6 box is the box, so 36.
    let union_area = matching(
        &mut db,
        plot.st_union("POLYGON((-1 -1, 5 -1, 5 5, -1 5, -1 -1))", 0)
            .st_area()
            .gt(lit(35.9999)),
    )
    .await;
    println!("plots whose union with a 6x6 box is about 36: {union_area:?}");

    // The hull of a square with a hole is the square itself, so 16.
    let hull = matching(&mut db, plot.st_convex_hull().st_area().gt(lit(15.9))).await;
    println!("plots whose convex hull is about 16: {hull:?}");

    // `geography` has a reduced surface, so the methods only it has live in a
    // separate trait. `ReorientObject` is the repair for a ring given the wrong
    // way round.
    let reoriented = matching(
        &mut db,
        Region::fields()
            .route()
            .st_reorient_object()
            .st_num_points()
            .eq(2),
    )
    .await;
    println!("routes that still have both points after reorienting: {reoriented:?}");

    // A shape computed in Rust can be written back to an existing row, along
    // the same encoding path the insert above used.
    let smaller = MssqlGeometry::from_geometry(
        Geometry::Polygon(Polygon::new(
            ring(&[(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0), (0.0, 0.0)]),
            vec![],
        )),
        0,
    );

    toasty::update!(region {
        plot: smaller.clone()
    })
    .exec(&mut db)
    .await?;

    let shrunk = matching(&mut db, plot.st_area().lt(lit(4.0001))).await;
    println!("plots now smaller than 4: {shrunk:?}");

    Ok(())
}

/// A closed ring from `(x, y)` pairs.
fn ring(points: &[(f64, f64)]) -> LineString<f64> {
    LineString::new(points.iter().map(|&(x, y)| Coord { x, y }).collect())
}

/// The ids of the rows matching `filter`, in order.
async fn matching(db: &mut toasty::Db, filter: toasty::stmt::Expr<bool>) -> Vec<i64> {
    let mut rows: Vec<Region> = Query::<List<Region>>::all()
        .filter(filter)
        .exec(db)
        .await
        .expect("the query must succeed");

    rows.sort_by_key(|row| row.id);

    rows.into_iter().map(|row| row.id).collect()
}
