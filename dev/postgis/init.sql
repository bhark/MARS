CREATE EXTENSION IF NOT EXISTS postgis;
CREATE SCHEMA IF NOT EXISTS e2e_source;

SELECT pg_create_logical_replication_slot('mars_local_slot', 'pgoutput')
  WHERE NOT EXISTS (SELECT 1 FROM pg_replication_slots WHERE slot_name = 'mars_local_slot');
