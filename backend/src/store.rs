use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;

use crate::types::{AnalyzeResponse, BBox, OsmWaterPoint};

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

pub async fn has_fresh_coverage(pool: &SqlitePool, bbox: BBox, now: DateTime<Utc>) -> Result<bool> {
    let rows = sqlx::query_as::<_, (f64, f64, f64, f64)>(
        "SELECT min_lat, min_lon, max_lat, max_lon
         FROM overpass_coverage
         WHERE expires_at > ?1",
    )
    .bind(now.to_rfc3339())
    .fetch_all(pool)
    .await
    .context("failed to read OSM coverage")?;

    Ok(rows
        .into_iter()
        .any(|(min_lat, min_lon, max_lat, max_lon)| {
            BBox {
                min_lat,
                min_lon,
                max_lat,
                max_lon,
            }
            .contains(bbox)
        }))
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

pub async fn refresh_water_points(
    pool: &SqlitePool,
    bbox: BBox,
    points: &[OsmWaterPoint],
    now: DateTime<Utc>,
    ttl: Duration,
) -> Result<()> {
    let refreshed_at = now.to_rfc3339();
    let expires_at = (now + ttl).to_rfc3339();
    let mut tx = pool.begin().await.context("failed to start cache update")?;

    for point in points {
        sqlx::query(
            "INSERT INTO water_points (osm_id, lat, lon, name, last_refresh)
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
        .context("failed to upsert water point")?;
    }

    sqlx::query(
        "DELETE FROM water_points
         WHERE lat >= ?1 AND lat <= ?2 AND lon >= ?3 AND lon <= ?4 AND last_refresh <> ?5",
    )
    .bind(bbox.min_lat)
    .bind(bbox.max_lat)
    .bind(bbox.min_lon)
    .bind(bbox.max_lon)
    .bind(&refreshed_at)
    .execute(&mut *tx)
    .await
    .context("failed to delete stale water points")?;

    sqlx::query(
        "INSERT INTO overpass_coverage
            (min_lat, min_lon, max_lat, max_lon, refreshed_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(bbox.min_lat)
    .bind(bbox.min_lon)
    .bind(bbox.max_lat)
    .bind(bbox.max_lon)
    .bind(&refreshed_at)
    .bind(expires_at)
    .execute(&mut *tx)
    .await
    .context("failed to write OSM coverage")?;

    tx.commit().await.context("failed to commit cache update")?;
    Ok(())
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
    async fn coverage_contains_requested_bbox_until_expiration() {
        let pool = pool().await;
        let now = Utc::now();
        let bbox = BBox {
            min_lat: 45.0,
            min_lon: 5.0,
            max_lat: 46.0,
            max_lon: 6.0,
        };

        refresh_water_points(&pool, bbox, &[], now, Duration::days(30))
            .await
            .unwrap();

        assert!(
            has_fresh_coverage(
                &pool,
                BBox {
                    min_lat: 45.2,
                    min_lon: 5.2,
                    max_lat: 45.8,
                    max_lon: 5.8,
                },
                now
            )
            .await
            .unwrap()
        );
        assert!(
            !has_fresh_coverage(&pool, bbox, now + Duration::days(31))
                .await
                .unwrap()
        );
    }
}
