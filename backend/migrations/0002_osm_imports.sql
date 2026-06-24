CREATE TABLE osm_imports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_url TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL,
    water_point_count INTEGER NOT NULL DEFAULT 0,
    error TEXT
);

CREATE INDEX idx_osm_imports_status_finished ON osm_imports(status, finished_at);

DROP TABLE IF EXISTS overpass_coverage;
