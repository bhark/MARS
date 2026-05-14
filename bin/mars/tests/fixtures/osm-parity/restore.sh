#!/usr/bin/env bash
# osm-parity restore: replay the pg_dump-format dump that was generated
# alongside the goldens. mounted into the postgres init container after
# seed.sql; runs once on the empty initdb. the `CREATE SCHEMA public;` line
# is stripped because the fresh init container already owns a public schema.
set -euo pipefail

DUMP="/opt/parity-fixture/osm-parity.sql.gz"
if [ ! -f "${DUMP}" ]; then
    echo "osm-parity restore: dump missing at ${DUMP}" >&2
    exit 1
fi

gunzip -c "${DUMP}" \
    | sed '/^CREATE SCHEMA public;$/d' \
    | psql -v ON_ERROR_STOP=1 -U "${POSTGRES_USER}" -d "${POSTGRES_DB}"
