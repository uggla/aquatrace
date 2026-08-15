# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.97.1
FROM docker.io/rust:${RUST_VERSION}-slim-trixie AS build

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock* ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --release

FROM docker.io/debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 10001 appuser \
    && mkdir -p /data \
    && chown -R appuser:appuser /data

USER appuser
WORKDIR /app
COPY --from=build /app/target/release/aquatrace-backend /usr/local/bin/aquatrace-backend
ENV DATABASE_URL=sqlite:/data/aquatrace.db \
    BIND_ADDR=0.0.0.0:3000 \
    MAX_UPLOAD_BYTES=10485760 \
    MAX_ANALYSIS_DISTANCE_M=500 \
    OSM_PBF_URL=https://download.geofabrik.de/europe-latest.osm.pbf \
    OSM_IMPORT_INTERVAL_SECONDS=1296000 \
    OSM_IMPORT_DIR=/data/osm \
    OSM_IMPORT_ON_STARTUP=true \
    ROUTE_CACHE_TTL_SECONDS=86400 \
    RUST_LOG=aquatrace_backend=info,tower_http=info
EXPOSE 3000
CMD ["aquatrace-backend"]
