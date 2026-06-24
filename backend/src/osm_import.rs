use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration as StdDuration, Instant},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use osmpbf::{Element, ElementReader};
use reqwest::header::USER_AGENT;
use sqlx::SqlitePool;
use tokio::{
    fs,
    io::AsyncWriteExt,
    time::{MissedTickBehavior, interval},
};
use tracing::{error, info, warn};

use crate::{
    Config, store,
    types::{OsmWaterPoint, RoutePoint},
};

const IMPORT_POLL_SECONDS: u64 = 3_600;
const DOWNLOAD_PROGRESS_LOG_SECONDS: u64 = 5;
const PBF_FILE_NAME: &str = "europe-latest.osm.pbf";

pub async fn ensure_initial_data(pool: &SqlitePool, config: &Config) -> Result<()> {
    if store::latest_successful_import(pool).await?.is_some()
        && store::has_water_points(pool).await?
    {
        return Ok(());
    }

    info!("no successful OSM import found; importing before serving requests");
    run_import(pool, config).await
}

pub fn spawn_import_scheduler(pool: SqlitePool, config: Arc<Config>) {
    tokio::spawn(async move {
        let mut ticker = interval(std::time::Duration::from_secs(IMPORT_POLL_SECONDS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;

            match import_due(&pool, &config).await {
                Ok(true) => {
                    if let Err(error) = run_import(&pool, &config).await {
                        error!("scheduled OSM import failed: {error:#}");
                    }
                }
                Ok(false) => {}
                Err(error) => warn!("failed to check OSM import freshness: {error:#}"),
            }
        }
    });
}

async fn import_due(pool: &SqlitePool, config: &Config) -> Result<bool> {
    let Some(latest) = store::latest_successful_import(pool).await? else {
        return Ok(config.osm_import_on_startup);
    };
    let Some(finished_at) = latest.finished_at else {
        return Ok(true);
    };

    Ok(Utc::now() - finished_at >= config.osm_import_interval)
}

pub async fn run_import(pool: &SqlitePool, config: &Config) -> Result<()> {
    let started_at = Utc::now();
    let import_id = store::start_import(pool, &config.osm_pbf_url, started_at).await?;
    let result = run_import_inner(pool, config, import_id).await;

    match &result {
        Ok(count) => {
            store::finish_import_success(pool, import_id, Utc::now(), *count).await?;
            info!(water_point_count = *count, "OSM import completed");
        }
        Err(error) => {
            let message = format!("{error:#}");
            store::finish_import_failure(pool, import_id, Utc::now(), &message).await?;
        }
    }

    result.map(|_| ())
}

async fn run_import_inner(pool: &SqlitePool, config: &Config, import_id: i64) -> Result<usize> {
    info!(
        import_id,
        source_url = %config.osm_pbf_url,
        import_dir = %config.osm_import_dir.display(),
        "starting OSM import"
    );
    let pbf_path = import_pbf_path(pool, config).await?;
    info!(
        import_id,
        pbf_path = %pbf_path.display(),
        "parsing OSM PBF dump"
    );
    let points = parse_drinking_water_points(pbf_path)
        .await
        .context("failed to parse OSM PBF dump")?;
    let count = points.len();
    info!(
        import_id,
        water_point_count = count,
        "finished parsing OSM PBF dump"
    );

    info!(
        import_id,
        water_point_count = count,
        "replacing SQLite water points"
    );
    store::replace_water_points(pool, &points, Utc::now()).await?;
    info!(import_id, "clearing route analysis cache after OSM import");
    store::clear_route_cache(pool).await?;

    info!(
        import_id,
        water_point_count = count,
        "activated imported OSM water points"
    );
    Ok(count)
}

async fn download_pbf(url: &str, import_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(import_dir)
        .await
        .with_context(|| format!("failed to create OSM import dir {}", import_dir.display()))?;

    let final_path = import_dir.join(PBF_FILE_NAME);
    let tmp_path = import_dir.join(format!("{PBF_FILE_NAME}.tmp"));
    if tmp_path.exists() {
        fs::remove_file(&tmp_path)
            .await
            .with_context(|| format!("failed to remove stale {}", tmp_path.display()))?;
    }

    let result = download_pbf_inner(url, &final_path, &tmp_path).await;
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path).await;
    }

    result.map(|_| final_path)
}

async fn import_pbf_path(pool: &SqlitePool, config: &Config) -> Result<PathBuf> {
    let local_path = config.osm_import_dir.join(PBF_FILE_NAME);
    if !store::has_water_points(pool).await? && local_pbf_is_fresh(&local_path, config).await? {
        info!(
            pbf_path = %local_path.display(),
            "using existing OSM PBF dump for initial import"
        );
        return Ok(local_path);
    }

    download_pbf(&config.osm_pbf_url, &config.osm_import_dir).await
}

async fn local_pbf_is_fresh(path: &Path, config: &Config) -> Result<bool> {
    let metadata = match fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to stat {}", path.display()));
        }
    };
    if !metadata.is_file() {
        return Ok(false);
    }

    let modified = metadata
        .modified()
        .with_context(|| format!("failed to read modification time for {}", path.display()))?;
    let modified_at: DateTime<Utc> = modified.into();
    let age = Utc::now() - modified_at;

    Ok(age <= config.osm_import_interval)
}

async fn download_pbf_inner(url: &str, final_path: &Path, tmp_path: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("AquaTrace/0.2")
        .build()
        .context("failed to build OSM download client")?;
    let mut response = client
        .get(url)
        .header(USER_AGENT, "AquaTrace/0.2")
        .send()
        .await
        .with_context(|| format!("failed to download OSM PBF from {url}"))?
        .error_for_status()
        .with_context(|| format!("OSM PBF download failed for {url}"))?;
    let content_length = response.content_length();

    let mut file = fs::File::create(&tmp_path)
        .await
        .with_context(|| format!("failed to create {}", tmp_path.display()))?;
    let mut downloaded_bytes = 0_u64;
    let mut last_progress_log = Instant::now();

    info!(
        url,
        target = %final_path.display(),
        total_bytes = content_length,
        "starting OSM PBF download"
    );

    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read OSM PBF chunk")?
    {
        downloaded_bytes += u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        file.write_all(&chunk)
            .await
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;

        if last_progress_log.elapsed() >= StdDuration::from_secs(DOWNLOAD_PROGRESS_LOG_SECONDS) {
            log_download_progress(downloaded_bytes, content_length);
            last_progress_log = Instant::now();
        }
    }
    file.flush()
        .await
        .with_context(|| format!("failed to flush {}", tmp_path.display()))?;
    drop(file);

    fs::rename(&tmp_path, &final_path).await.with_context(|| {
        format!(
            "failed to move {} to {}",
            tmp_path.display(),
            final_path.display()
        )
    })?;

    info!(
        downloaded_bytes,
        target = %final_path.display(),
        "finished OSM PBF download"
    );

    Ok(())
}

fn log_download_progress(downloaded_bytes: u64, content_length: Option<u64>) {
    match content_length {
        Some(total_bytes) if total_bytes > 0 => {
            let percent = downloaded_bytes as f64 * 100.0 / total_bytes as f64;
            info!(
                downloaded_bytes,
                total_bytes,
                percent = format_args!("{percent:.1}"),
                "OSM PBF download in progress"
            );
        }
        _ => {
            info!(downloaded_bytes, "OSM PBF download in progress");
        }
    }
}

async fn parse_drinking_water_points(path: PathBuf) -> Result<Vec<OsmWaterPoint>> {
    tokio::task::spawn_blocking(move || parse_drinking_water_points_blocking(&path))
        .await
        .context("OSM PBF parser task failed")?
}

fn parse_drinking_water_points_blocking(path: &Path) -> Result<Vec<OsmWaterPoint>> {
    let reader = ElementReader::from_path(path)
        .with_context(|| format!("failed to open {}", path.display()))?;

    let points = reader
        .par_map_reduce(
            |element| {
                let mut points = HashMap::new();
                if let Some(point) = water_point_from_element(element) {
                    points.insert(point.osm_id, point);
                }
                points
            },
            HashMap::new,
            |mut left, right| {
                left.extend(right);
                left
            },
        )
        .with_context(|| format!("failed to read {}", path.display()))?;

    Ok(points.into_values().collect())
}

fn water_point_from_element(element: Element<'_>) -> Option<OsmWaterPoint> {
    match element {
        Element::Node(node) => water_point_from_parts(
            node.id(),
            RoutePoint {
                lat: node.lat(),
                lon: node.lon(),
                ele: None,
            },
            node.tags(),
        ),
        Element::DenseNode(node) => water_point_from_parts(
            node.id(),
            RoutePoint {
                lat: node.lat(),
                lon: node.lon(),
                ele: None,
            },
            node.tags(),
        ),
        Element::Way(_) | Element::Relation(_) => None,
    }
}

fn water_point_from_parts<'a>(
    osm_id: i64,
    point: RoutePoint,
    tags: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<OsmWaterPoint> {
    let mut is_drinking_water = false;
    let mut name = None;

    for (key, value) in tags {
        match key {
            "amenity" if value == "drinking_water" => is_drinking_water = true,
            "name" => name = Some(value.to_owned()),
            _ => {}
        }
    }

    is_drinking_water.then_some(OsmWaterPoint {
        osm_id,
        lat: point.lat,
        lon: point.lon,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_strict_drinking_water_node_tags() {
        let point = water_point_from_parts(
            12,
            RoutePoint {
                lat: 45.0,
                lon: 5.0,
                ele: None,
            },
            [("amenity", "drinking_water"), ("name", "Village Tap")],
        )
        .unwrap();

        assert_eq!(point.osm_id, 12);
        assert_eq!(point.name.as_deref(), Some("Village Tap"));
    }

    #[test]
    fn ignores_non_strict_water_tags() {
        let point = water_point_from_parts(
            12,
            RoutePoint {
                lat: 45.0,
                lon: 5.0,
                ele: None,
            },
            [("drinking_water", "yes"), ("name", "Spring")],
        );

        assert!(point.is_none());
    }
}
