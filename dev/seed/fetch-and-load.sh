#!/bin/sh
set -eu

OSM_BBOX="${OSM_BBOX:-9.5,55.6,10.7,56.0}"
PG_DSN="${PG_DSN:-postgres://mars:mars@mars-postgis/mars}"
CACHE_DIR="/cache"
OSM_FILE="${CACHE_DIR}/osm-extract.osm"
PBF_FILE="${CACHE_DIR}/osm-extract.osm.pbf"

mkdir -p "${CACHE_DIR}"

if [ -f "${PBF_FILE}" ]; then
    echo "seed: using cached PBF ${PBF_FILE}"
else
    echo "seed: fetching OSM data for bbox ${OSM_BBOX}"
    # substitute bbox into query and fetch
    sed "s/{{bbox}}/${OSM_BBOX}/g" /work/overpass-query.txt > /tmp/overpass-query.txt
    curl -fsSL -X POST \
        -d @/tmp/overpass-query.txt \
        -H "Content-Type: text/plain" \
        --retry 3 \
        --retry-delay 5 \
        "https://overpass-api.de/api/interpreter" \
        -o "${OSM_FILE}"

    echo "seed: converting to PBF"
    osmium cat "${OSM_FILE}" -o "${PBF_FILE}"
fi

echo "seed: loading into PostGIS"
osm2pgsql \
    --create \
    --slim \
    --drop \
    --latlong \
    --schema=e2e_source \
    --output=flex \
    --style=/work/osm-mapping.lua \
    -d "${PG_DSN}" \
    "${PBF_FILE}"

echo "seed: reprojecting to EPSG:25832"
for tbl in land water settlements roads buildings waterways; do
    psql "${PG_DSN}" -c "
        ALTER TABLE e2e_source.${tbl}
        ALTER COLUMN geom TYPE geometry(Geometry,25832)
        USING ST_Transform(geom, 25832);
    "
done

echo "seed: sanity-checking loaded tables"
for tbl in land water settlements roads buildings waterways; do
    count=$(psql "${PG_DSN}" -Atc "SELECT COUNT(*) FROM e2e_source.${tbl}")
    if [ "$count" -eq 0 ]; then
        echo "seed: ERROR: table e2e_source.${tbl} is empty"
        exit 1
    fi
    echo "seed: e2e_source.${tbl}: ${count} rows"
done

echo "seed: creating publication"
psql "${PG_DSN}" -c "CREATE PUBLICATION mars_local_pub FOR ALL TABLES IN SCHEMA e2e_source"

echo "seed: done"
