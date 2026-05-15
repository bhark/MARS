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
SELECT pg_create_logical_replication_slot('mars_e2e_slot', 'pgoutput');
