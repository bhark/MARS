-- (re)create the pgoutput publication + replication slot the compiler reads
-- via mars_e2e_pub / mars_e2e_slot. idempotent: drops any prior pub/slot of
-- the same name so reseeds work cleanly.
DROP PUBLICATION IF EXISTS mars_e2e_pub;
SELECT pg_drop_replication_slot('mars_e2e_slot')
WHERE EXISTS (SELECT 1 FROM pg_replication_slots WHERE slot_name = 'mars_e2e_slot');
CREATE PUBLICATION mars_e2e_pub FOR TABLE
  e2e_source.land,
  e2e_source.water,
  e2e_source.settlements,
  e2e_source.roads,
  e2e_source.buildings,
  e2e_source.waterways;
-- mars requires REPLICA IDENTITY FULL on every published table so
-- update/delete carry the OLD row (incl. geometry) for dirty-page derivation.
ALTER TABLE e2e_source.land       REPLICA IDENTITY FULL;
ALTER TABLE e2e_source.water      REPLICA IDENTITY FULL;
ALTER TABLE e2e_source.settlements REPLICA IDENTITY FULL;
ALTER TABLE e2e_source.roads      REPLICA IDENTITY FULL;
ALTER TABLE e2e_source.buildings  REPLICA IDENTITY FULL;
ALTER TABLE e2e_source.waterways  REPLICA IDENTITY FULL;
SELECT pg_create_logical_replication_slot('mars_e2e_slot', 'pgoutput');
