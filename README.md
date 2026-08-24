# AquaTrace

AquaTrace analyzes GPX routes and finds nearby OpenStreetMap points tagged
`amenity=drinking_water`, including toilets tagged `amenity=toilets` with
`drinking_water=yes`.
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

The backend only starts serving requests once both the Europe PBF extract and an imported SQLite
dataset are available. Missing data is downloaded and imported synchronously. When an existing
dataset is usable but stale, the backend starts immediately and refreshes it in the background.
In local development, the default import directory is `data/osm`.

To download and import a fresh extract regardless of its age, use:

```bash
cargo run -- --force-osm-download
```

## Backend Configuration

```text
DATABASE_URL=sqlite:/data/aquatrace.db
BIND_ADDR=0.0.0.0:3000
MAX_UPLOAD_BYTES=10485760
MAX_ANALYSIS_DISTANCE_M=500
GPX_LOG_DIR=data/log
GPX_LOG_RETENTION_DAYS=90
GPX_LOG_MAX_BYTES=1073741824
OSM_PBF_URL=https://download.geofabrik.de/europe-latest.osm.pbf
OSM_IMPORT_INTERVAL_SECONDS=1296000
OSM_IMPORT_DIR=data/osm
OSM_IMPORT_ON_STARTUP=true
ROUTE_CACHE_TTL_SECONDS=86400
GOOGLE_MAPS_API_KEY=
```

`GOOGLE_MAPS_API_KEY` is optional. When set, the frontend asks the backend for confirmed Google
Street View links after displaying the route analysis. Enable the Street View Static API for the
Google Cloud project and restrict the key to that API and, in production, to the backend server IP.
Metadata lookups use a 500 ms timeout and never block or change the GPX analysis response.

The Europe PBF extract is large. Keep enough free disk space in the import directory for the
download and SQLite import workspace. Docker/Compose overrides `OSM_IMPORT_DIR` to `/data/osm`.
`OSM_IMPORT_ON_STARTUP=false` skips an optional refresh when both the local PBF and SQLite dataset
are already usable; it never permits the API to start with either one missing. Download, both PBF
parsing passes, and SQLite replacement progress are logged every five seconds during an import.
Eligible OSM ways are resolved to a representative point during the second pass.

Completed GPX submissions are compressed as `.gpx.gz` files under `GPX_LOG_DIR/valid` or
`GPX_LOG_DIR/error`. The backend removes files older than `GPX_LOG_RETENTION_DAYS`, then removes
the oldest remaining files as needed to keep both directories under `GPX_LOG_MAX_BYTES` in total.
Archiving is best effort and never changes the API response. These files contain user location data;
restrict access to the archive directory and choose retention settings appropriate for your privacy
requirements. Docker/Compose stores the archive under the existing `/data` volume.

## API

```http
POST /api/analyze
```

Multipart body:

```text
file=<route.gpx>
```

The response includes route metrics, route points for map display, and drinking water points sorted by kilometer.

Street View availability can be requested separately after an analysis:

```http
POST /api/street-view
Content-Type: application/json
```

```json
{
  "locations": [
    { "id": "node:123", "lat": 45.123456, "lon": 5.123456 }
  ]
}
```

The response preserves the identifiers and input order. A `street_view_url` property is included
only when Google confirms a panorama within 50 metres. A request accepts at most 2,000 locations.

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
