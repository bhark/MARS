-- osm-parity: per-layer materialised tables fanned out from planet_osm_*.
--
-- two reasons the harness can't bind layers directly to planet_osm_*:
--   1. multiple layers fan out from the same physical table with different
--      filters and attribute lists. table-shaped bindings on the same `from`
--      collapse to one binding id and reject conflicting attribute shapes;
--      `sql:` bindings are unique by SELECT hash but the runtime's manifest
--      validation does not yet probe them, so layers backed exclusively by
--      `sql:` are flagged as missing.
--   2. osm2pgsql encodes multipolygon relations with negative osm_ids, which
--      the source decoder rejects. abs(osm_id) folds those into the positive
--      space; collisions are theoretically possible between a way id N and a
--      relation id -N but never observed in this dataset.
--
-- using plain CREATE VIEW would be lighter, but the compiler's snapshot
-- probe needs the `tableoid`/`ctid` system columns - views do not expose
-- those, so we materialise as physical tables.

CREATE TABLE parity_landuse AS
    SELECT abs(osm_id)::bigint AS fid, way, landuse
      FROM planet_osm_polygon
      WHERE landuse IS NOT NULL;

CREATE TABLE parity_water AS
    SELECT abs(osm_id)::bigint AS fid, way
      FROM planet_osm_polygon
      WHERE "natural" = 'water' OR water IS NOT NULL;

CREATE TABLE parity_waterways AS
    SELECT abs(osm_id)::bigint AS fid, way, waterway
      FROM planet_osm_line
      WHERE waterway IN ('stream','river','canal');

CREATE TABLE parity_roads_minor AS
    SELECT abs(osm_id)::bigint AS fid, way
      FROM planet_osm_line
      WHERE highway IN ('residential','tertiary','unclassified','service','living_street');

CREATE TABLE parity_roads_major AS
    SELECT abs(osm_id)::bigint AS fid, way, highway
      FROM planet_osm_roads
      WHERE highway IN ('motorway','trunk','primary','secondary','motorway_link','trunk_link','primary_link','secondary_link');

CREATE TABLE parity_buildings AS
    SELECT abs(osm_id)::bigint AS fid, way
      FROM planet_osm_polygon
      WHERE building IS NOT NULL;

CREATE TABLE parity_boundary AS
    SELECT abs(osm_id)::bigint AS fid, way
      FROM planet_osm_line
      WHERE boundary = 'administrative' AND admin_level = '2';

CREATE TABLE parity_places AS
    SELECT abs(osm_id)::bigint AS fid, way, place
      FROM planet_osm_point
      WHERE place IN ('town','village','hamlet');
