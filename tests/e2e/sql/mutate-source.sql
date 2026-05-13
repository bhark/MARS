-- one cycle's worth of mutations across two layers. b_incremental applies
-- this Job after capturing the baseline manifest version + render; the
-- assertion is that mars_manifest_version advances and the re-render bytes
-- differ from the baseline. all mutations land inside the render bbox so
-- they actually affect the visible image.
BEGIN;

-- INSERT: a new poi well inside the render bbox.
INSERT INTO e2e_source.poi (kind, name, geom) VALUES
    ('summit', 'Test Cycle Insert',
     ST_SetSRID(ST_MakePoint(866000, 6112000), 25832));

-- UPDATE: rename one of the seeded poi rows so the change-feed sees a row
-- mutation that re-renders the label.
UPDATE e2e_source.poi
SET name = 'Valby Bakke (renamed)'
WHERE name = 'Valby Bakke';

-- DELETE: drop the northernmost seeded poi.
DELETE FROM e2e_source.poi WHERE name = 'Husum';

COMMIT;
