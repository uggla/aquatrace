# AquaTrace

AquaTrace analyzes GPX routes and finds nearby OpenStreetMap points tagged `amenity=drinking_water`.

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

## Backend Configuration

```text
DATABASE_URL=sqlite:/data/aquatrace.db
BIND_ADDR=0.0.0.0:3000
OVERPASS_URL=https://overpass-api.de/api/interpreter
MAX_UPLOAD_BYTES=10485760
MAX_ANALYSIS_DISTANCE_M=500
ROUTE_CACHE_TTL_SECONDS=86400
OSM_CACHE_TTL_SECONDS=2592000
```

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

External Overpass calls are mocked in tests.

## Attribution

The map displays:

```text
© OpenStreetMap contributors © OpenTopoMap
```
