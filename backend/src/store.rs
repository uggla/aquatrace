use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;

use crate::types::{AnalyzeResponse, BBox, OsmWaterPoint};

pub const IMPORT_STATUS_RUNNING: &str = "running";
pub const IMPORT_STATUS_SUCCESS: &str = "success";
pub const IMPORT_STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, PartialEq)]
pub struct OsmImportStatus {
    pub id: i64,
    pub source_url: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub water_point_count: i64,
    pub error: Option<String>,
}

pub async fn get_route_cache(
    pool: &SqlitePool,
    gpx_hash: &str,
    now: DateTime<Utc>,
) -> Result<Option<AnalyzeResponse>> {
    let analysis_json = sqlx::query_scalar::<_, String>(
        "SELECT analysis_json FROM routes WHERE gpx_hash = ?1 AND expires_at > ?2",
    )
    .bind(gpx_hash)
    .bind(now.to_rfc3339())
    .fetch_optional(pool)
    .await
    .context("failed to read route cache")?;

    analysis_json
        .map(|json| serde_json::from_str(&json).context("failed to decode route cache"))
        .transpose()
}

pub async fn put_route_cache(
    pool: &SqlitePool,
    gpx_hash: &str,
    analysis: &AnalyzeResponse,
    now: DateTime<Utc>,
    ttl: Duration,
) -> Result<()> {
    let analysis_json = serde_json::to_string(analysis).context("failed to encode analysis")?;
    let expires_at = now + ttl;

    sqlx::query(
        "INSERT INTO routes (gpx_hash, created_at, expires_at, analysis_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(gpx_hash) DO UPDATE SET
            created_at = excluded.created_at,
            expires_at = excluded.expires_at,
            analysis_json = excluded.analysis_json",
    )
    .bind(gpx_hash)
    .bind(now.to_rfc3339())
    .bind(expires_at.to_rfc3339())
    .bind(analysis_json)
    .execute(pool)
    .await
    .context("failed to write route cache")?;

    Ok(())
}

pub async fn clear_route_cache(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM routes")
        .execute(pool)
        .await
        .context("failed to clear route cache")?;

    Ok(())
}

pub async fn water_points_in_bbox(pool: &SqlitePool, bbox: BBox) -> Result<Vec<OsmWaterPoint>> {
    let rows = sqlx::query_as::<_, (i64, f64, f64, Option<String>)>(
        "SELECT osm_id, lat, lon, name
         FROM water_points
         WHERE lat >= ?1 AND lat <= ?2 AND lon >= ?3 AND lon <= ?4",
    )
    .bind(bbox.min_lat)
    .bind(bbox.max_lat)
    .bind(bbox.min_lon)
    .bind(bbox.max_lon)
    .fetch_all(pool)
    .await
    .context("failed to read water point cache")?;

    Ok(rows
        .into_iter()
        .map(|(osm_id, lat, lon, name)| OsmWaterPoint {
            osm_id,
            lat,
            lon,
            name,
        })
        .collect())
}

pub async fn has_water_points(pool: &SqlitePool) -> Result<bool> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM water_points")
        .fetch_one(pool)
        .await
        .context("failed to count water points")?;

    Ok(count > 0)
}

pub async fn latest_successful_import(pool: &SqlitePool) -> Result<Option<OsmImportStatus>> {
    let row = sqlx::query_as::<
        _,
        (
            i64,
            String,
            String,
            Option<String>,
            String,
            i64,
            Option<String>,
        ),
    >(
        "SELECT id, source_url, started_at, finished_at, status, water_point_count, error
         FROM osm_imports
         WHERE status = ?1
         ORDER BY finished_at DESC, id DESC
         LIMIT 1",
    )
    .bind(IMPORT_STATUS_SUCCESS)
    .fetch_optional(pool)
    .await
    .context("failed to read latest OSM import")?;

    row.map(import_status_from_row).transpose()
}

pub async fn start_import(pool: &SqlitePool, source_url: &str, now: DateTime<Utc>) -> Result<i64> {
    let result = sqlx::query(
        "INSERT INTO osm_imports (source_url, started_at, status)
         VALUES (?1, ?2, ?3)",
    )
    .bind(source_url)
    .bind(now.to_rfc3339())
    .bind(IMPORT_STATUS_RUNNING)
    .execute(pool)
    .await
    .context("failed to record OSM import start")?;

    Ok(result.last_insert_rowid())
}

pub async fn finish_import_success(
    pool: &SqlitePool,
    import_id: i64,
    finished_at: DateTime<Utc>,
    water_point_count: usize,
) -> Result<()> {
    sqlx::query(
        "UPDATE osm_imports
         SET finished_at = ?1, status = ?2, water_point_count = ?3, error = NULL
         WHERE id = ?4",
    )
    .bind(finished_at.to_rfc3339())
    .bind(IMPORT_STATUS_SUCCESS)
    .bind(i64::try_from(water_point_count).context("water point count does not fit in i64")?)
    .bind(import_id)
    .execute(pool)
    .await
    .context("failed to record OSM import success")?;

    Ok(())
}

pub async fn finish_import_failure(
    pool: &SqlitePool,
    import_id: i64,
    finished_at: DateTime<Utc>,
    error: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE osm_imports
         SET finished_at = ?1, status = ?2, error = ?3
         WHERE id = ?4",
    )
    .bind(finished_at.to_rfc3339())
    .bind(IMPORT_STATUS_FAILED)
    .bind(error)
    .bind(import_id)
    .execute(pool)
    .await
    .context("failed to record OSM import failure")?;

    Ok(())
}

pub async fn replace_water_points(
    pool: &SqlitePool,
    points: &[OsmWaterPoint],
    now: DateTime<Utc>,
) -> Result<()> {
    let refreshed_at = now.to_rfc3339();
    let mut tx = pool
        .begin()
        .await
        .context("failed to start OSM import update")?;

    sqlx::query("DROP TABLE IF EXISTS water_points_next")
        .execute(&mut *tx)
        .await
        .context("failed to drop stale import table")?;

    sqlx::query(
        "CREATE TABLE water_points_next (
            osm_id INTEGER PRIMARY KEY,
            lat REAL NOT NULL,
            lon REAL NOT NULL,
            name TEXT,
            last_refresh TEXT NOT NULL
        )",
    )
    .execute(&mut *tx)
    .await
    .context("failed to create import table")?;

    for point in points {
        sqlx::query(
            "INSERT INTO water_points_next (osm_id, lat, lon, name, last_refresh)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(osm_id) DO UPDATE SET
                lat = excluded.lat,
                lon = excluded.lon,
                name = excluded.name,
                last_refresh = excluded.last_refresh",
        )
        .bind(point.osm_id)
        .bind(point.lat)
        .bind(point.lon)
        .bind(&point.name)
        .bind(&refreshed_at)
        .execute(&mut *tx)
        .await
        .context("failed to insert imported water point")?;
    }

    sqlx::query("DROP TABLE water_points")
        .execute(&mut *tx)
        .await
        .context("failed to drop old water points")?;
    sqlx::query("ALTER TABLE water_points_next RENAME TO water_points")
        .execute(&mut *tx)
        .await
        .context("failed to activate imported water points")?;
    sqlx::query("CREATE INDEX idx_water_points_lat_lon ON water_points(lat, lon)")
        .execute(&mut *tx)
        .await
        .context("failed to index imported water points")?;
    tx.commit()
        .await
        .context("failed to commit OSM import update")?;
    Ok(())
}

fn import_status_from_row(
    row: (
        i64,
        String,
        String,
        Option<String>,
        String,
        i64,
        Option<String>,
    ),
) -> Result<OsmImportStatus> {
    let (id, source_url, started_at, finished_at, status, water_point_count, error) = row;

    Ok(OsmImportStatus {
        id,
        source_url,
        started_at: parse_rfc3339_utc(&started_at)?,
        finished_at: finished_at.as_deref().map(parse_rfc3339_utc).transpose()?,
        status,
        water_point_count,
        error,
    })
}

fn parse_rfc3339_utc(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid timestamp in OSM import table: {value}"))?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn route_cache_expires() {
        let pool = pool().await;
        let now = Utc::now();
        let analysis = AnalyzeResponse {
            route: crate::gpx_parser::summarize_route(vec![
                crate::types::RoutePoint {
                    lat: 45.0,
                    lon: 5.0,
                    ele: None,
                },
                crate::types::RoutePoint {
                    lat: 45.1,
                    lon: 5.0,
                    ele: None,
                },
            ]),
            water_points: vec![],
        };

        put_route_cache(&pool, "hash", &analysis, now, Duration::seconds(1))
            .await
            .unwrap();
        assert!(get_route_cache(&pool, "hash", now).await.unwrap().is_some());
        assert!(
            get_route_cache(&pool, "hash", now + Duration::seconds(2))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn replacing_water_points_swaps_dataset() {
        let pool = pool().await;
        let now = Utc::now();

        assert!(!has_water_points(&pool).await.unwrap());

        replace_water_points(
            &pool,
            &[OsmWaterPoint {
                osm_id: 1,
                lat: 45.0,
                lon: 5.0,
                name: Some("Old".to_owned()),
            }],
            now,
        )
        .await
        .unwrap();
        replace_water_points(
            &pool,
            &[OsmWaterPoint {
                osm_id: 2,
                lat: 46.0,
                lon: 6.0,
                name: Some("New".to_owned()),
            }],
            now,
        )
        .await
        .unwrap();

        let points = water_points_in_bbox(
            &pool,
            BBox {
                min_lat: 44.0,
                min_lon: 4.0,
                max_lat: 47.0,
                max_lon: 7.0,
            },
        )
        .await
        .unwrap();

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].osm_id, 2);
        assert_eq!(points[0].name.as_deref(), Some("New"));
        assert!(has_water_points(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn route_cache_can_be_cleared_after_import() {
        let pool = pool().await;
        let now = Utc::now();
        let analysis = AnalyzeResponse {
            route: crate::gpx_parser::summarize_route(vec![
                crate::types::RoutePoint {
                    lat: 45.0,
                    lon: 5.0,
                    ele: None,
                },
                crate::types::RoutePoint {
                    lat: 45.1,
                    lon: 5.0,
                    ele: None,
                },
            ]),
            water_points: vec![],
        };

        put_route_cache(&pool, "hash", &analysis, now, Duration::days(1))
            .await
            .unwrap();
        clear_route_cache(&pool).await.unwrap();

        assert!(get_route_cache(&pool, "hash", now).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn import_status_tracks_latest_success() {
        let pool = pool().await;
        let now = Utc::now();
        let import_id = start_import(&pool, "https://example.test/europe.osm.pbf", now)
            .await
            .unwrap();

        finish_import_success(&pool, import_id, now + Duration::seconds(5), 42)
            .await
            .unwrap();

        let latest = latest_successful_import(&pool).await.unwrap().unwrap();
        assert_eq!(latest.id, import_id);
        assert_eq!(latest.water_point_count, 42);
        assert_eq!(latest.status, IMPORT_STATUS_SUCCESS);
    }
}
