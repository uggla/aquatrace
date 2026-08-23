ALTER TABLE water_points RENAME TO water_points_legacy;

CREATE TABLE water_points (
    osm_type TEXT NOT NULL CHECK (osm_type IN ('node', 'way')),
    osm_id INTEGER NOT NULL,
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    name TEXT,
    last_refresh TEXT NOT NULL,
    PRIMARY KEY (osm_type, osm_id)
);

INSERT INTO water_points (osm_type, osm_id, lat, lon, name, last_refresh)
SELECT 'node', osm_id, lat, lon, name, last_refresh
FROM water_points_legacy;

DROP TABLE water_points_legacy;

CREATE INDEX idx_water_points_lat_lon ON water_points(lat, lon);

ALTER TABLE osm_imports
ADD COLUMN dataset_version INTEGER NOT NULL DEFAULT 1;
