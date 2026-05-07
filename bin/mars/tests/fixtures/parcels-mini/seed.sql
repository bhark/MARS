-- parcels-mini: tiny synthetic municipal-management style fixture.
-- one polygon table, single geometry column, one classifier column.
-- schema:
--   mars_diff.parcels(gid int4 pk, kind text not null, geom polygon EPSG:25832)
--   kind in ('park','road','water'); seven simple non-overlapping squares.
-- bbox of seeded geometry: x in [0, 1000], y in [0, 1000]. fits in cell (0,0).

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE SCHEMA mars_diff;

CREATE TABLE mars_diff.parcels (
    gid  INT4 PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    geom geometry(Polygon, 25832) NOT NULL
);

INSERT INTO mars_diff.parcels (gid, kind, name, geom) VALUES
    (1, 'park',  'Alpha', ST_GeomFromText('POLYGON((100 100, 300 100, 300 300, 100 300, 100 100))', 25832)),
    (2, 'park',  'Beta',  ST_GeomFromText('POLYGON((400 100, 600 100, 600 300, 400 300, 400 100))', 25832)),
    (3, 'road',  '',      ST_GeomFromText('POLYGON((100 400, 900 400, 900 500, 100 500, 100 400))', 25832)),
    (4, 'water', 'Lake',  ST_GeomFromText('POLYGON((100 600, 400 600, 400 900, 100 900, 100 600))', 25832)),
    (5, 'water', '',      ST_GeomFromText('POLYGON((500 600, 800 600, 800 900, 500 900, 500 600))', 25832)),
    (6, 'park',  'Gamma', ST_GeomFromText('POLYGON((700 100, 900 100, 900 300, 700 300, 700 100))', 25832)),
    (7, 'road',  '',      ST_GeomFromText('POLYGON((100 550, 200 550, 200 590, 100 590, 100 550))', 25832));
