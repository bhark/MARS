-- e2e: derive the e2e_source schema from osm2pgsql's planet_osm_* tables in
-- the shared parity OSM dump. mirrors the rationale in
-- tests/parity/fixtures/osm/02-views.sql:
--   - materialised tables (not views) so the runtime compiler's snapshot
--     probe can read tableoid/ctid;
--   - row_number() generates a synthetic dense id space because abs(osm_id)
--     is not unique (osm2pgsql emits one row per polygon ring, plus
--     potential way/relation id collisions after abs()), and the e2e
--     contract requires `id bigint PRIMARY KEY`.
-- geometries are reprojected from EPSG:3857 (osm2pgsql native) to 25832 so
-- the e2e service contract (native_crs: EPSG:25832) holds without runtime
-- reprojection on every query.

CREATE SCHEMA IF NOT EXISTS e2e_source;

CREATE TABLE e2e_source.land AS
    SELECT row_number() OVER ()::bigint AS id,
           ST_Transform(way, 25832) AS geom
      FROM planet_osm_polygon
     WHERE landuse IS NOT NULL;
ALTER TABLE e2e_source.land ADD PRIMARY KEY (id);
CREATE INDEX land_geom_gix ON e2e_source.land USING GIST (geom);

CREATE TABLE e2e_source.water AS
    SELECT row_number() OVER ()::bigint AS id,
           ST_Transform(way, 25832) AS geom
      FROM planet_osm_polygon
     WHERE "natural" = 'water' OR water IS NOT NULL;
ALTER TABLE e2e_source.water ADD PRIMARY KEY (id);
CREATE INDEX water_geom_gix ON e2e_source.water USING GIST (geom);

CREATE TABLE e2e_source.settlements AS
    SELECT row_number() OVER ()::bigint AS id,
           ST_Transform(way, 25832) AS geom
      FROM planet_osm_polygon
     WHERE landuse IN ('residential','commercial','industrial')
        OR place IN ('village','town','hamlet','suburb');
ALTER TABLE e2e_source.settlements ADD PRIMARY KEY (id);
CREATE INDEX settlements_geom_gix ON e2e_source.settlements USING GIST (geom);

CREATE TABLE e2e_source.roads AS
    SELECT row_number() OVER ()::bigint AS id,
           ST_Transform(way, 25832) AS geom,
           CASE WHEN highway IN ('motorway','trunk','primary','secondary')
                THEN 'major' ELSE 'minor' END AS kind
      FROM planet_osm_line
     WHERE highway IS NOT NULL;
ALTER TABLE e2e_source.roads ADD PRIMARY KEY (id);
CREATE INDEX roads_geom_gix ON e2e_source.roads USING GIST (geom);

-- buildings.status is synthetic: OSM has no equivalent attribute, so we
-- derive it deterministically from id so the `status='temporary'` class
-- filter (service.yaml + marsservice.yaml.tmpl) always matches some rows.
CREATE TABLE e2e_source.buildings AS
    SELECT row_number() OVER ()::bigint AS id,
           ST_Transform(way, 25832) AS geom,
           coalesce(building, 'yes') AS kind
      FROM planet_osm_polygon
     WHERE building IS NOT NULL;
ALTER TABLE e2e_source.buildings ADD COLUMN status text;
UPDATE e2e_source.buildings
   SET status = CASE WHEN id % 50 = 0 THEN 'temporary' ELSE 'permanent' END;
ALTER TABLE e2e_source.buildings ALTER COLUMN status SET NOT NULL;
ALTER TABLE e2e_source.buildings ADD PRIMARY KEY (id);
CREATE INDEX buildings_geom_gix ON e2e_source.buildings USING GIST (geom);

CREATE TABLE e2e_source.waterways AS
    SELECT row_number() OVER ()::bigint AS id,
           ST_Transform(way, 25832) AS geom,
           CASE waterway WHEN 'river' THEN 'wide'
                         WHEN 'canal' THEN 'wide'
                         ELSE 'narrow' END AS width_class
      FROM planet_osm_line
     WHERE waterway IN ('stream','river','canal','drain','ditch');
ALTER TABLE e2e_source.waterways ADD PRIMARY KEY (id);
CREATE INDEX waterways_geom_gix ON e2e_source.waterways USING GIST (geom);
