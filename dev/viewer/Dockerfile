# vendor JS/CSS deps at build time so the dev viewer works offline
# (planes, captive portals, flaky CDNs). pinned versions live below.
ARG OL_VERSION=10.4.0
ARG PROJ4_VERSION=2.15.0

FROM docker.io/alpine:3.21 AS assets
ARG OL_VERSION
ARG PROJ4_VERSION
RUN apk add --no-cache curl \
 && mkdir -p /assets/ol /assets/proj4 \
 && curl -fsSL -o /assets/ol/ol.js \
        "https://cdn.jsdelivr.net/npm/ol@${OL_VERSION}/dist/ol.js" \
 && curl -fsSL -o /assets/ol/ol.css \
        "https://cdn.jsdelivr.net/npm/ol@${OL_VERSION}/ol.css" \
 && curl -fsSL -o /assets/proj4/proj4.js \
        "https://cdnjs.cloudflare.com/ajax/libs/proj4js/${PROJ4_VERSION}/proj4.js"

FROM docker.io/nginx:alpine
COPY nginx.conf /etc/nginx/nginx.conf
COPY index.html /usr/share/nginx/html/index.html
COPY --from=assets /assets/ol/    /usr/share/nginx/html/vendor/ol/
COPY --from=assets /assets/proj4/ /usr/share/nginx/html/vendor/proj4/

EXPOSE 80
