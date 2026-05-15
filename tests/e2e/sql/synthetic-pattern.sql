-- synthetic polygon table co-located with the rest of e2e_source. used by the
-- image-pattern fill scenario to render a single rectangle styled with
-- FillPaint::Image, proving the compiler -> manifest -> runtime -> renderer
-- seam wires bitmap fills end-to-end.
CREATE TABLE IF NOT EXISTS e2e_source.pattern_zone (
    id   serial PRIMARY KEY,
    geom geometry(Polygon, 25832) NOT NULL
);

CREATE INDEX IF NOT EXISTS pattern_zone_geom_gix ON e2e_source.pattern_zone USING GIST (geom);

-- mars requires REPLICA IDENTITY FULL on every published table.
ALTER TABLE e2e_source.pattern_zone REPLICA IDENTITY FULL;

-- one rectangle inside the render bbox [536000,5210000,548000,5235000];
-- sized so a 16px tile repeats clearly within a 256px frame.
INSERT INTO e2e_source.pattern_zone (geom) VALUES (
    ST_SetSRID(ST_MakeEnvelope(539000, 5219000, 545000, 5228000), 25832)
) ON CONFLICT DO NOTHING;

-- ride the same change-feed as the dump tables; pattern matches synthetic-poi.sql.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_publication_tables
        WHERE pubname = 'mars_e2e_pub' AND schemaname = 'e2e_source' AND tablename = 'pattern_zone'
    ) THEN
        EXECUTE 'ALTER PUBLICATION mars_e2e_pub ADD TABLE e2e_source.pattern_zone';
    END IF;
END$$;
