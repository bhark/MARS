-- assert that the fixture loaded the six required tables. raises if any are
-- missing so a malformed dump fails fast before the compiler is wired up.
DO $$
DECLARE
  missing text[];
BEGIN
  SELECT array_agg(name) INTO missing
  FROM (VALUES
    ('e2e_source.land'),
    ('e2e_source.water'),
    ('e2e_source.settlements'),
    ('e2e_source.roads'),
    ('e2e_source.buildings'),
    ('e2e_source.waterways')
  ) AS required(name)
  WHERE to_regclass(name) IS NULL;
  IF missing IS NOT NULL THEN
    RAISE EXCEPTION 'fixture missing required tables: %', array_to_string(missing, ', ');
  END IF;
END $$;
