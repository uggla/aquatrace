CREATE TABLE routes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    gpx_hash TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    analysis_json TEXT NOT NULL
);

CREATE INDEX idx_routes_hash_expires ON routes(gpx_hash, expires_at);

CREATE TABLE water_points (
    osm_id INTEGER PRIMARY KEY,
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    name TEXT,
    last_refresh TEXT NOT NULL
);

CREATE INDEX idx_water_points_lat_lon ON water_points(lat, lon);

CREATE TABLE overpass_coverage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    min_lat REAL NOT NULL,
    min_lon REAL NOT NULL,
    max_lat REAL NOT NULL,
    max_lon REAL NOT NULL,
    refreshed_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

CREATE INDEX idx_overpass_coverage_expires ON overpass_coverage(expires_at);
