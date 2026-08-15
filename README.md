# AquaTrace

AquaTrace analyzes GPX routes and finds nearby OpenStreetMap points tagged `amenity=drinking_water`.
The backend imports an OpenStreetMap Europe extract into SQLite and serves route analysis from local data.

## Stack

- Frontend: Vite, TypeScript, PicoCSS, Leaflet, OpenTopoMap.
- Backend: Rust 2024, Axum, SQLx, SQLite, reqwest, gpx, rstar.
- Deployment: two containers, matching the `lovebin` pattern.

## Run With Docker Or Podman

```bash
docker compose up --build
```

The app is available at:

```text
http://localhost:8080
```

The backend is private on the Compose network. The frontend serves static files and proxies `/api/*` to `backend:3000`.

SQLite data is persisted on the host at:

```text
backend/data/aquatrace.db
```

## Local Development

Backend:

```bash
cd backend
DATABASE_URL=sqlite:data/aquatrace.db cargo run
```

Frontend:

```bash
cd frontend
npm install
npm run dev
```

Vite proxies `/api` to `http://127.0.0.1:3000`.

The first backend startup downloads and imports the Europe OSM extract before serving requests.
In local development, the default import directory is `data/osm`. For local UI work without the
import, start the backend with `OSM_IMPORT_ON_STARTUP=false`.

## Backend Configuration

```text
DATABASE_URL=sqlite:/data/aquatrace.db
BIND_ADDR=0.0.0.0:3000
MAX_UPLOAD_BYTES=10485760
MAX_ANALYSIS_DISTANCE_M=500
OSM_PBF_URL=https://download.geofabrik.de/europe-latest.osm.pbf
OSM_IMPORT_INTERVAL_SECONDS=1296000
OSM_IMPORT_DIR=data/osm
OSM_IMPORT_ON_STARTUP=true
ROUTE_CACHE_TTL_SECONDS=86400
```

The Europe PBF extract is large. Keep enough free disk space in the import directory for the
download and SQLite import workspace. Docker/Compose overrides `OSM_IMPORT_DIR` to `/data/osm`.

## API

```http
POST /api/analyze
```

Multipart body:

```text
file=<route.gpx>
```

The response includes route metrics, route points for map display, and drinking water points sorted by kilometer.

## Tests

```bash
cd backend
cargo test
```

External OpenStreetMap imports are mocked or bypassed in tests.

## Attribution

The map displays:

```text
© OpenStreetMap contributors © OpenTopoMap
```

The Rust logo is owned by the Rust Foundation and used under the
[Creative Commons Attribution license](https://creativecommons.org/licenses/by/4.0/).
