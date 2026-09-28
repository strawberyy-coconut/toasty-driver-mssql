//! SQL Server spatial columns, against a live server.
//!
//! The codec in `src/spatial/codec.rs` is covered by its own unit tests, using
//! payloads captured from a real server — but those tests cannot tell a correct
//! encoder from a *self-consistently wrong* one. An encoder that wrote the
//! coordinates in the wrong order would still round-trip through our own
//! decoder, and would still decode the captured vectors correctly, because
//! neither direction would have changed. Only the server can settle it.
//!
//! The rest of the file does the same for the method vocabulary: every spatial
//! method is run once, because a method whose SQL the server rejects has no
//! answer to check.
//!
//! The columns being `geometry`/`geography` is covered incidentally: if either
//! had fallen back to `varbinary`, the `STGeometryType` calls in the comparison
//! would not resolve.

use geo_types::{Coord, Geometry, LineString, Polygon};
use toasty::db::Driver as _;
use toasty::stmt::{List, Query};
use toasty_driver_mssql::{
    ClientContext, Mssql, MssqlGeography, MssqlGeographyExt as _, MssqlGeometry,
    MssqlGeometryExt as _, MssqlSpatial as _, lit,
};

mod common;

#[derive(Debug, toasty::Model)]
struct Place {
    #[key]
    id: i64,

    shape: MssqlGeometry,
    area: MssqlGeography,
}

async fn setup(database: &str) -> (toasty::Db, ClientContext) {
    let context = common::context(database);
    let driver = Mssql::new(context.clone());
    driver.reset_db().await.expect("reset_db must succeed");

    let db = toasty::Db::builder()
        .models(toasty::models!(Place))
        .build(driver)
        .await
        .expect("build must succeed");

    db.push_schema().await.expect("push_schema must succeed");

    (db, context)
}

/// A shape computed in Rust is written to the server and read back as the shape
/// it was meant to be.
///
/// This is what [`MssqlGeometry::from_geometry`] is for: without the codec the
/// only way to get a spatial value was to read one out of a column, so a shape
/// could be copied but never computed.
#[tokio::test]
async fn a_geometry_computed_in_rust_is_the_shape_the_server_reads() {
    let (mut db, context) = setup("toasty_geo").await;

    // A polygon with a hole, so the encoding has to get the exterior/interior
    // figure attributes and both rings right. The area is 4x4 minus 1x1.
    let exterior = LineString::new(vec![
        Coord { x: 0.0, y: 0.0 },
        Coord { x: 4.0, y: 0.0 },
        Coord { x: 4.0, y: 4.0 },
        Coord { x: 0.0, y: 4.0 },
        Coord { x: 0.0, y: 0.0 },
    ]);
    let hole = LineString::new(vec![
        Coord { x: 1.0, y: 1.0 },
        Coord { x: 2.0, y: 1.0 },
        Coord { x: 2.0, y: 2.0 },
        Coord { x: 1.0, y: 2.0 },
        Coord { x: 1.0, y: 1.0 },
    ]);
    let shape = Geometry::Polygon(Polygon::new(exterior, vec![hole]));

    // The geography column takes a line string: it has no winding rule to get
    // wrong, and the point here is the *encoding*, not the left-hand rule.
    let route = Geometry::LineString(LineString::new(vec![
        Coord { x: 1.0, y: 2.0 },
        Coord { x: 3.0, y: 4.0 },
        Coord { x: 5.0, y: 6.0 },
    ]));

    let created = toasty::create!(Place {
        id: 10_i64,
        shape: MssqlGeometry::from_geometry(shape.clone(), 4326),
        area: MssqlGeography::from_geometry(route.clone(), 4326),
    })
    .exec(&mut db)
    .await
    .expect("the server must accept a payload this driver encoded");

    // Our decoder reading our encoder's bytes: the payload survived the round
    // trip through TDS, the column and back. This is the only cover for the
    // model-level read path, which the codec's own tests do not reach.
    assert_eq!(created.shape.srid(), Some(4326));
    assert_eq!(
        created.shape.to_geometry().expect("must decode"),
        shape,
        "the polygon did not survive the round trip"
    );
    assert_eq!(
        created.area.to_geometry().expect("must decode"),
        route,
        "the line string did not survive the round trip"
    );

    // And then ask SQL Server itself whether our bytes are the shape we meant.
    //
    // Note the geography WKT: it is written `(lat long)`, the opposite order to
    // the `Coord` it came from. `geography` stores longitude first, and
    // `geo_types::Coord` is `(x = longitude, y = latitude)`, so `Coord { x: 1,
    // y: 2 }` is the WKT's `2 1`. Writing the WKT in the same order as the Rust
    // coordinates is the mistake this check first caught.
    //
    // `geometry` has no `=` operator — it is a CLR type, not a comparable one —
    // so the payloads are compared as `varbinary`, which is also what makes this
    // stricter than any check of the decoded shape.
    Mssql::new(context)
        .execute_raw(
            "IF NOT EXISTS (
                 SELECT 1 FROM [places]
                 WHERE [id] = 10
                   AND [shape].STGeometryType() = N'Polygon'
                   AND [shape].STSrid = 4326
                   AND [shape].STNumInteriorRing() = 1
                   AND ABS([shape].STArea() - 15.0) < 0.000001
                   AND [area].STGeometryType() = N'LineString'
                   AND [area].STSrid = 4326
                   AND CONVERT(varbinary(max), [shape]) = CONVERT(varbinary(max),
                       geometry::STGeomFromText(N'POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 2, 1 1))', 4326))
                   AND CONVERT(varbinary(max), [area]) = CONVERT(varbinary(max),
                       geography::STGeomFromText(N'LINESTRING(2 1, 4 3, 6 5)', 4326))
             )
             THROW 50000, 'the payload this driver encoded is not the shape the server reads', 1;",
        )
        .await
        .expect("our encoding must be byte-for-byte what the server produces");
}

/// Ids of the places matching `filter`.
async fn ids(db: &mut toasty::Db, filter: toasty::stmt::Expr<bool>) -> Vec<i64> {
    let mut rows: Vec<Place> = Query::<List<Place>>::all()
        .filter(filter)
        .exec(db)
        .await
        .expect("query must succeed");

    rows.sort_by_key(|row| row.id);

    rows.into_iter().map(|row| row.id).collect()
}

/// The spatial methods, driven through the query builder.
///
/// These are *methods* — `col.STArea()` — not functions, because that is the
/// only form T-SQL offers for them. So this is the test that the carrier can
/// express a method call at all, and that the second operand of the two-shape
/// methods is a constructor for the column's *own* type: a `geography` column
/// must be handed `geography::STGeomFromText`, not the `geometry` one.
#[tokio::test]
async fn spatial_methods_are_callable_from_a_filter() {
    let (mut db, _context) = setup("toasty_geo_funcs").await;

    // A 4x4 square with a 1x1 hole: area 15, perimeter 16, ten points.
    let exterior = LineString::new(vec![
        Coord { x: 0.0, y: 0.0 },
        Coord { x: 4.0, y: 0.0 },
        Coord { x: 4.0, y: 4.0 },
        Coord { x: 0.0, y: 4.0 },
        Coord { x: 0.0, y: 0.0 },
    ]);
    let hole = LineString::new(vec![
        Coord { x: 1.0, y: 1.0 },
        Coord { x: 2.0, y: 1.0 },
        Coord { x: 2.0, y: 2.0 },
        Coord { x: 1.0, y: 2.0 },
        Coord { x: 1.0, y: 1.0 },
    ]);

    toasty::create!(Place {
        id: 1_i64,
        shape: MssqlGeometry::from_geometry(
            Geometry::Polygon(Polygon::new(exterior, vec![hole])),
            0,
        ),
        area: MssqlGeography::from_geometry(
            Geometry::LineString(LineString::new(vec![
                Coord { x: 1.0, y: 2.0 },
                Coord { x: 3.0, y: 4.0 },
            ])),
            4326,
        ),
    })
    .exec(&mut db)
    .await
    .expect("create");

    let shape = Place::fields().shape();

    let area = shape
        .st_area()
        .gt(lit(14.9999))
        .and(shape.st_area().lt(lit(15.0001)));
    assert_eq!(ids(&mut db, area).await, [1]);

    // Ring lengths are *summed*, unlike areas which are signed: the 4x4
    // exterior is 16 and the 1x1 hole is another 4, so a hole subtracts from
    // `STArea` and adds to `STLength`.
    let perimeter = shape
        .st_length()
        .gt(lit(19.9999))
        .and(shape.st_length().lt(lit(20.0001)));
    assert_eq!(ids(&mut db, perimeter).await, [1]);

    // Five points on each ring.
    assert_eq!(ids(&mut db, shape.st_num_points().eq(10)).await, [1]);
    assert_eq!(
        ids(&mut db, shape.st_geometry_type().eq("Polygon")).await,
        [1]
    );

    // `STAsText` answers the server's own rendering of the value. This file
    // builds under `spatial` alone, so the string functions that could read
    // such a result are not compiled in; the assertion only claims that the
    // call renders and runs.
    assert_eq!(ids(&mut db, shape.st_as_text().gt("")).await, [1]);

    // Nothing here is the empty geometry.
    assert_eq!(ids(&mut db, shape.st_is_empty()).await, Vec::<i64>::new());

    // A point in the body, outside the hole.
    assert_eq!(
        ids(&mut db, shape.st_intersects("POINT(0.5 0.5)", 0)).await,
        [1]
    );
    assert_eq!(
        ids(&mut db, shape.st_contains("POINT(0.5 0.5)", 0)).await,
        [1]
    );

    // A point inside the hole is within the polygon's extent but not in the
    // polygon, which is what makes the hole a hole.
    assert_eq!(
        ids(&mut db, shape.st_contains("POINT(1.5 1.5)", 0)).await,
        Vec::<i64>::new()
    );

    // The inverse of `STContains`, and a shape that encloses the whole square.
    let enclosing = "POLYGON((-1 -1, 5 -1, 5 5, -1 5, -1 -1))";
    assert_eq!(ids(&mut db, shape.st_within(enclosing, 0)).await, [1]);

    // A geometry two units to the side is two units away from the square's edge.
    let distance = shape
        .st_distance("POINT(6 2)", 0)
        .gt(lit(1.9999))
        .and(shape.st_distance("POINT(6 2)", 0).lt(lit(2.0001)));
    assert_eq!(ids(&mut db, distance).await, [1]);
}

/// A square and a line, for the breadth test below.
async fn seed_shapes(db: &mut toasty::Db) {
    let exterior = LineString::new(vec![
        Coord { x: 0.0, y: 0.0 },
        Coord { x: 4.0, y: 0.0 },
        Coord { x: 4.0, y: 4.0 },
        Coord { x: 0.0, y: 4.0 },
        Coord { x: 0.0, y: 0.0 },
    ]);
    let hole = LineString::new(vec![
        Coord { x: 1.0, y: 1.0 },
        Coord { x: 2.0, y: 1.0 },
        Coord { x: 2.0, y: 2.0 },
        Coord { x: 1.0, y: 2.0 },
        Coord { x: 1.0, y: 1.0 },
    ]);

    toasty::create!(Place {
        id: 1_i64,
        shape: MssqlGeometry::from_geometry(
            Geometry::Polygon(Polygon::new(exterior, vec![hole])),
            0,
        ),
        // A line, not a polygon: a geography polygon's meaning depends on its
        // ring orientation, which this test has no business asserting.
        area: MssqlGeography::from_geometry(
            Geometry::LineString(LineString::new(vec![
                Coord { x: 1.0, y: 2.0 },
                Coord { x: 3.0, y: 4.0 },
            ])),
            4326,
        ),
    })
    .exec(db)
    .await
    .expect("create");
}

/// Every spatial method, once, to prove the server accepts the SQL it renders
/// to.
///
/// As in `funcs.rs`, this asserts only that nothing is *rejected*. A method that
/// answers a shape cannot stand in a `WHERE` on its own — a geometry has no `=`
/// operator — so each is chained to something that answers a number, which is
/// also the composition path proven below.
#[tokio::test]
async fn every_spatial_method_is_accepted_by_the_server() {
    let (mut db, _context) = setup("toasty_geo_accepted").await;
    seed_shapes(&mut db).await;

    let shape = Place::fields().shape();
    let area = Place::fields().area();

    let square = "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))";
    let enclosing = "POLYGON((-1 -1, 5 -1, 5 5, -1 5, -1 -1))";
    let segment = "LINESTRING(0 0, 0 1)";

    // `st_num_points() > -1` is true for any non-null shape, so chaining it
    // tests that the shape arrived without asserting what it is.
    let cases: Vec<(&str, toasty::stmt::Expr<bool>)> = vec![
        ("st_intersects", shape.st_intersects(square, 0)),
        ("st_contains", shape.st_contains("POINT(0.5 0.5)", 0)),
        ("st_within", shape.st_within(enclosing, 0)),
        ("st_equals", shape.st_equals(square, 0)),
        ("st_disjoint", shape.st_disjoint("POINT(10 10)", 0)),
        ("st_overlaps", shape.st_overlaps(square, 0)),
        ("st_touches", shape.st_touches("POINT(0 2)", 0)),
        ("st_crosses", shape.st_crosses("LINESTRING(-1 2, 5 2)", 0)),
        ("st_is_valid", shape.st_is_valid()),
        ("st_is_simple", shape.st_is_simple()),
        ("st_is_empty", shape.st_is_empty()),
        ("st_area", shape.st_area().gt(lit(-1.0))),
        ("st_length", shape.st_length().gt(lit(-1.0))),
        ("st_num_points", shape.st_num_points().gt(-1)),
        (
            "st_distance",
            shape.st_distance("POINT(6 2)", 0).gt(lit(-1.0)),
        ),
        ("st_geometry_type", shape.st_geometry_type().gt("")),
        ("st_as_text", shape.st_as_text().gt("")),
        ("to_string", shape.to_string().gt("")),
        (
            "st_union",
            shape.st_union(enclosing, 0).st_num_points().gt(-1),
        ),
        (
            "st_intersection",
            shape.st_intersection(square, 0).st_num_points().gt(-1),
        ),
        (
            "st_difference",
            shape.st_difference(square, 0).st_num_points().gt(-1),
        ),
        (
            "st_sym_difference",
            shape.st_sym_difference(square, 0).st_num_points().gt(-1),
        ),
        ("st_buffer", shape.st_buffer(0.5).st_num_points().gt(-1)),
        ("st_envelope", shape.st_envelope().st_area().gt(lit(-1.0))),
        (
            "st_convex_hull",
            shape.st_convex_hull().st_area().gt(lit(-1.0)),
        ),
        ("st_boundary", shape.st_boundary().st_num_points().gt(-1)),
        ("st_centroid", shape.st_centroid().st_num_points().gt(-1)),
        (
            "st_point_on_surface",
            shape.st_point_on_surface().st_num_points().gt(-1),
        ),
        (
            "st_make_valid",
            shape.st_make_valid().st_area().gt(lit(-1.0)),
        ),
        (
            "st_intersects (geography)",
            area.st_intersects(segment, 4326),
        ),
        (
            "st_contains (geography)",
            area.st_contains("POINT(2 3)", 4326),
        ),
        (
            "st_within (geography)",
            area.st_within("LINESTRING(-1 -1, 5 5)", 4326),
        ),
        (
            "st_equals (geography)",
            area.st_equals("LINESTRING(2 1, 4 3)", 4326),
        ),
        (
            "st_disjoint (geography)",
            area.st_disjoint("POINT(10 10)", 4326),
        ),
        (
            "st_overlaps (geography)",
            area.st_overlaps("LINESTRING(3 4, 5 6)", 4326),
        ),
        ("st_is_valid (geography)", area.st_is_valid()),
        ("st_area (geography)", area.st_area().gt(lit(-1.0))),
        ("st_length (geography)", area.st_length().gt(lit(-1.0))),
        (
            "st_distance (geography)",
            area.st_distance("POINT(10 10)", 4326).gt(lit(-1.0)),
        ),
        (
            "st_geometry_type (geography)",
            area.st_geometry_type().gt(""),
        ),
        (
            "st_union (geography)",
            area.st_union(segment, 4326).st_num_points().gt(-1),
        ),
        (
            "st_buffer (geography)",
            area.st_buffer(1.0).st_num_points().gt(-1),
        ),
        (
            "st_reorient_object",
            area.st_reorient_object().st_num_points().gt(-1),
        ),
    ];

    let mut failures = Vec::new();

    for (name, filter) in cases {
        let result: Result<Vec<Place>, _> = Query::<List<Place>>::all()
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

/// A call can be the receiver of another, which is the only way a shape-returning
/// method says anything.
#[tokio::test]
async fn a_call_can_be_another_calls_receiver() {
    let (mut db, _context) = setup("toasty_geo_nested").await;
    seed_shapes(&mut db).await;

    let shape = Place::fields().shape();

    // The union of a 16-unit square (15 after its hole) with a 6x6 box is 36.
    let union_area = shape
        .st_union("POLYGON((-1 -1, 5 -1, 5 5, -1 5, -1 -1))", 0)
        .st_area()
        .gt(lit(35.9999))
        .and(
            shape
                .st_union("POLYGON((-1 -1, 5 -1, 5 5, -1 5, -1 -1))", 0)
                .st_area()
                .lt(lit(36.0001)),
        );
    assert_eq!(ids(&mut db, union_area).await, [1]);

    // And a shape-returning method chained to a *description* of what it
    // answered: the centroid of a polygon is a point. The string functions
    // would read that further, but they live behind their own feature.
    let described = shape.st_centroid().st_geometry_type().eq("Point");
    assert_eq!(ids(&mut db, described).await, [1]);
}
