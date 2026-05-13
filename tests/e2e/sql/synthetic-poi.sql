-- synthetic point layer co-located with the rest of e2e_source. the external
-- fixture dump only ships polygon/line tables, so the kind loader runs this
-- after create-replication.sql to give the renderer's symbol/ + label/ paths
-- something to draw. table is added to the existing pgoutput publication so
-- inserts/updates/deletes on it ride the same change-feed as the dump tables.
CREATE TABLE IF NOT EXISTS e2e_source.poi (
    id   serial PRIMARY KEY,
    kind text NOT NULL,
    name text NOT NULL,
    geom geometry(Point, 25832) NOT NULL
);

CREATE INDEX IF NOT EXISTS poi_geom_gix ON e2e_source.poi USING GIST (geom);

-- seed rows inside the render bbox [850000,6090000,895000,6145000]
INSERT INTO e2e_source.poi (kind, name, geom) VALUES
    ('summit', 'Bispebjerg',  ST_SetSRID(ST_MakePoint(858000, 6100000), 25832)),
    ('summit', 'Valby Bakke', ST_SetSRID(ST_MakePoint(872000, 6118000), 25832)),
    ('summit', 'Brønshøj',    ST_SetSRID(ST_MakePoint(880000, 6132000), 25832)),
    ('summit', 'Husum',       ST_SetSRID(ST_MakePoint(889000, 6140000), 25832))
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
