-- osm-parity: prelude executed by the postgres init container before the
-- compressed sql dump is restored. ensures postgis is available in `public`
-- (the dump's planet_osm_* tables reference public.geometry types).
CREATE EXTENSION IF NOT EXISTS postgis;
