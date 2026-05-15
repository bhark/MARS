-- one cycle's worth of mutations across the poi layer. b_incremental applies
-- this Job after capturing the baseline manifest version + render; the
-- assertion is that mars_manifest_version advances and the re-render bytes
-- differ from the baseline. all mutations land inside the render bbox so
-- they actually affect the visible image.
BEGIN;

-- INSERT: a new poi well inside the render bbox.
INSERT INTO e2e_source.poi (kind, name, geom) VALUES
    ('summit', 'epsilon',
     ST_SetSRID(ST_MakePoint(540000, 5220000), 25832));

-- UPDATE: rename one of the seeded poi rows so the change-feed sees a row
-- mutation that re-renders the label.
UPDATE e2e_source.poi
SET name = 'beta-renamed'
WHERE name = 'beta';

-- DELETE: drop the northernmost seeded poi.
DELETE FROM e2e_source.poi WHERE name = 'delta';

COMMIT;
