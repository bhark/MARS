#!/bin/sh
set -eu

# wait for postgis to accept connections (compose depends_on
# service_healthy gates on pg_isready, but only the postgis volume
# init has run by then - belt + braces).
i=0
until pg_isready -h postgis -p 5432 -U mars -d mars -q; do
    i=$((i+1))
    if [ "$i" -ge 60 ]; then
        echo "fixture-loader: timeout waiting for postgis" >&2
        exit 1
    fi
    sleep 2
done

psql "$PG_DSN" -v ON_ERROR_STOP=1 -c "CREATE EXTENSION IF NOT EXISTS postgis"
# idempotent reload: drop the e2e_source schema and any prior pub/slot
# (create-replication.sql does the latter) so reruns converge cleanly.
psql "$PG_DSN" -v ON_ERROR_STOP=1 -c "DROP SCHEMA IF EXISTS e2e_source CASCADE"
gzip -dc /fixture/dump.sql.gz | psql "$PG_DSN" -v ON_ERROR_STOP=1
psql "$PG_DSN" -v ON_ERROR_STOP=1 -f /sql/assert-fixture.sql
psql "$PG_DSN" -v ON_ERROR_STOP=1 -f /sql/create-replication.sql
