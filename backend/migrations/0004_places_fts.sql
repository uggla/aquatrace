CREATE TABLE places (
    id INTEGER PRIMARY KEY,
    osm_id INTEGER NOT NULL UNIQUE,
    name TEXT NOT NULL,
    place_type TEXT NOT NULL CHECK (place_type IN ('city', 'town', 'village', 'hamlet')),
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    search_text TEXT NOT NULL,
    last_refresh TEXT NOT NULL
);

CREATE INDEX idx_places_lat_lon ON places(lat, lon);

CREATE VIRTUAL TABLE places_fts USING fts5(
    name,
    search_text,
    place_id UNINDEXED,
    tokenize = 'unicode61 remove_diacritics 2'
);
