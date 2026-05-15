-- one cycle's worth of mutations across the settlements layer. b_incremental
-- applies this Job after capturing the baseline manifest version + render;
-- the assertion is that mars_manifest_version advances and the re-render
-- bytes differ from the baseline.
--
-- settlements is targeted (not poi) because the test's render bbox + 512px
-- image resolves to scale_denom ~88_582, which sits in the `mid` band
-- [25_000, 250_000). settlements has a `mid` source so it actually renders
-- at this scale; poi is bound to `hi` [12_500, 25_000) and is invisible
-- here. all mutated polygons land inside the bbox so they affect pixels.
--
-- synthetic ids start at 9_000_000_001 to stay well above the row_number()-
-- derived range produced by derive-e2e.sql.
BEGIN;

INSERT INTO e2e_source.settlements (id, geom) VALUES
    (9000000001, ST_SetSRID(ST_GeomFromText(
        'POLYGON((538000 5215000, 540000 5215000, 540000 5217000, 538000 5217000, 538000 5215000))'), 25832)),
    (9000000002, ST_SetSRID(ST_GeomFromText(
        'POLYGON((541000 5219000, 543000 5219000, 543000 5221000, 541000 5221000, 541000 5219000))'), 25832)),
    (9000000003, ST_SetSRID(ST_GeomFromText(
        'POLYGON((544000 5223000, 546000 5223000, 546000 5225000, 544000 5225000, 544000 5223000))'), 25832));

-- UPDATE: move one of the new polygons so the change-feed sees a row mutation
-- whose pre- and post-image differ on geom (REPLICA IDENTITY FULL carries the
-- old row).
UPDATE e2e_source.settlements
   SET geom = ST_SetSRID(ST_GeomFromText(
       'POLYGON((541000 5226000, 543000 5226000, 543000 5228000, 541000 5228000, 541000 5226000))'), 25832)
 WHERE id = 9000000002;

-- DELETE: drop one of the new polygons.
DELETE FROM e2e_source.settlements WHERE id = 9000000003;

COMMIT;
