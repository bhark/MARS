-- synthetic point layer co-located with the rest of e2e_source. the upstream
-- OSM dump only ships planet_osm_* polygon/line/point tables and derive-e2e.sql
-- materialises polygon/line layers; the kind loader runs this after
-- create-replication.sql to give the renderer's symbol/ + label/ paths
-- something to draw. table is added to the existing pgoutput publication so
-- inserts/updates/deletes on it ride the same change-feed as the dump tables.
CREATE TABLE IF NOT EXISTS e2e_source.poi (
    id   serial PRIMARY KEY,
    kind text NOT NULL,
    name text NOT NULL,
    geom geometry(Point, 25832) NOT NULL
);

CREATE INDEX IF NOT EXISTS poi_geom_gix ON e2e_source.poi USING GIST (geom);

-- seed rows inside the render bbox [536000,5210000,548000,5235000].
-- names are neutral (alpha/beta/gamma/delta) — the tests key off these.
INSERT INTO e2e_source.poi (kind, name, geom) VALUES
    ('summit', 'alpha', ST_SetSRID(ST_MakePoint(538000, 5216000), 25832)),
    ('summit', 'beta',  ST_SetSRID(ST_MakePoint(541000, 5222000), 25832)),
    ('summit', 'gamma', ST_SetSRID(ST_MakePoint(544000, 5228000), 25832)),
    ('summit', 'delta', ST_SetSRID(ST_MakePoint(547000, 5233000), 25832))
ON CONFLICT DO NOTHING;

-- ALTER PUBLICATION ADD TABLE is not idempotent before pg 16; we ALWAYS drop
-- and re-create the publication in create-replication.sql, so adding poi here
-- via ALTER is safe (publication exists, slot exists, poi not yet included).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_publication_tables
        WHERE pubname = 'mars_e2e_pub' AND schemaname = 'e2e_source' AND tablename = 'poi'
    ) THEN
        EXECUTE 'ALTER PUBLICATION mars_e2e_pub ADD TABLE e2e_source.poi';
    END IF;
END$$;
