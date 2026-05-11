FROM docker.io/debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        osm2pgsql \
        osmium-tool \
        curl \
        ca-certificates \
        postgresql-client \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /work
COPY fetch-and-load.sh overpass-query.txt osm-mapping.lua ./

ENTRYPOINT ["/bin/sh", "/work/fetch-and-load.sh"]
