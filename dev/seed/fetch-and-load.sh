#!/bin/sh
set -eu

# overpass bbox order is (south,west,north,east)
OSM_BBOX="${OSM_BBOX:-55.6,9.5,56.0,10.7}"
PG_DSN="${PG_DSN:-postgres://mars:mars@postgis/mars}"
PUBLICATION="${PUBLICATION:-mars_local_pub}"
# must match `SCHEMA` in osm-mapping.lua
SCHEMA="e2e_source"
CACHE_DIR="/cache"
OSM_FILE="${CACHE_DIR}/osm-extract.osm"
PBF_FILE="${CACHE_DIR}/osm-extract.osm.pbf"

mkdir -p "${CACHE_DIR}"

# wait for postgres before any psql/osm2pgsql work. compose's depends_on
# service_healthy already gates this, but keep the loop so the script stays
# usable outside the compose flow.
PG_HOST=$(echo "${PG_DSN}" | sed -nE 's|^postgres://[^@]+@([^/:]+).*|\1|p')
i=0
until pg_isready -h "${PG_HOST}" -U mars -d mars -q; do
    i=$((i+1))
    if [ "$i" -ge 60 ]; then
        echo "seed: timeout waiting for ${PG_HOST}" >&2
        exit 1
    fi
    sleep 2
done

if [ -f "${PBF_FILE}" ]; then
    echo "seed: using cached PBF ${PBF_FILE}"
else
    echo "seed: fetching OSM data for bbox ${OSM_BBOX}"
    sed "s/{{bbox}}/${OSM_BBOX}/g" /work/overpass-query.txt > /tmp/overpass-query.txt
    curl -fsSL -X POST \
        -d @/tmp/overpass-query.txt \
        -H "Content-Type: text/plain" \
        --retry 3 \
        --retry-delay 5 \
        "https://overpass-api.de/api/interpreter" \
        -o "${OSM_FILE}"

    # osm2pgsql requires input sorted by type+id; overpass uses quadtile order
    echo "seed: converting to sorted PBF"
    osmium sort "${OSM_FILE}" -o "${PBF_FILE}"
fi

# idempotent reset: drop publication + schema before osm2pgsql --create.
# postgres has no CREATE PUBLICATION IF NOT EXISTS, and osm2pgsql --create
# fails if its output tables already exist - so the cleanest path is a
# clean slate every time. the OSM extract itself stays cached.
echo "seed: resetting target schema and publication"
psql "${PG_DSN}" -v ON_ERROR_STOP=1 <<SQL
DROP PUBLICATION IF EXISTS ${PUBLICATION};
DROP SCHEMA IF EXISTS ${SCHEMA} CASCADE;
CREATE SCHEMA ${SCHEMA};
SQL

echo "seed: loading into PostGIS (projecting to EPSG:25832 on load)"
# osm2pgsql 1.8 lacks --schema; the lua mapping pins each table to ${SCHEMA}.
osm2pgsql \
    --create \
    --slim \
    --drop \
    --output=flex \
    --style=/work/osm-mapping.lua \
    -d "${PG_DSN}" \
    "${PBF_FILE}"

echo "seed: sanity-checking loaded tables"
for tbl in land water settlements roads buildings waterways; do
    count=$(psql "${PG_DSN}" -Atc "SELECT COUNT(*) FROM ${SCHEMA}.${tbl}")
    if [ "$count" -eq 0 ]; then
        echo "seed: ERROR: table ${SCHEMA}.${tbl} is empty"
        exit 1
    fi
    echo "seed: ${SCHEMA}.${tbl}: ${count} rows"
done

echo "seed: creating publication"
psql "${PG_DSN}" -v ON_ERROR_STOP=1 \
    -c "CREATE PUBLICATION ${PUBLICATION} FOR TABLES IN SCHEMA ${SCHEMA}"

echo "seed: done"
