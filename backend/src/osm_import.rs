use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Result as IoResult},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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
    time::{Instant as TokioInstant, MissedTickBehavior, interval, interval_at},
};
use tracing::{error, info, warn};

use crate::{
    Config, store,
    types::{OsmWaterPoint, RoutePoint},
};

const IMPORT_POLL_SECONDS: u64 = 3_600;
const PROGRESS_LOG_SECONDS: u64 = 5;
const PBF_FILE_NAME: &str = "europe-latest.osm.pbf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalPbfState {
    Absent,
    Fresh,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupAction {
    BlockingImport,
    BackgroundRefresh,
    Ready,
}

fn startup_action(
    pbf_state: LocalPbfState,
    database_is_usable: bool,
    force_download: bool,
    import_on_startup: bool,
    latest_import_is_stale: bool,
) -> StartupAction {
    if pbf_state == LocalPbfState::Absent || !database_is_usable {
        StartupAction::BlockingImport
    } else if force_download
        || (import_on_startup && (pbf_state == LocalPbfState::Stale || latest_import_is_stale))
    {
        StartupAction::BackgroundRefresh
    } else {
        StartupAction::Ready
    }
}

pub async fn prepare_initial_data(
    pool: &SqlitePool,
    config: &Config,
    force_download: bool,
) -> Result<bool> {
    let pbf_path = config.osm_import_dir.join(PBF_FILE_NAME);
    let pbf_state = local_pbf_state(&pbf_path, config).await?;
    let has_water_points = store::has_water_points(pool).await?;
    let latest_import = store::latest_successful_import(pool).await?;
    let database_is_usable = has_water_points && latest_import.is_some();

    let action = startup_action(
        pbf_state,
        database_is_usable,
        force_download,
        config.osm_import_on_startup,
        successful_import_is_stale(latest_import.as_ref(), config),
    );

    match action {
        StartupAction::BlockingImport => {
            info!(
                pbf_state = ?pbf_state,
                database_is_usable,
                force_download,
                "OSM data is not ready; completing import before serving requests"
            );
            run_import(pool, config, force_download).await?;
            Ok(false)
        }
        StartupAction::BackgroundRefresh => {
            info!(
                pbf_state = ?pbf_state,
                force_download,
                "OSM data is usable; scheduling refresh in the background"
            );
            Ok(true)
        }
        StartupAction::Ready => Ok(false),
    }
}

pub fn spawn_import_scheduler(pool: SqlitePool, config: Arc<Config>, refresh_on_start: bool) {
    tokio::spawn(async move {
        let mut ticker = interval(std::time::Duration::from_secs(IMPORT_POLL_SECONDS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut forced_refresh_pending = refresh_on_start;
        ticker.tick().await;

        if forced_refresh_pending {
            match run_import(&pool, &config, true).await {
                Ok(()) => forced_refresh_pending = false,
                Err(error) => error!("scheduled OSM import failed: {error:#}"),
            }
        }

        loop {
            ticker.tick().await;

            let import_is_due = if forced_refresh_pending {
                Ok(true)
            } else {
                import_due(&pool, &config).await
            };

            match import_is_due {
                Ok(true) => match run_import(&pool, &config, forced_refresh_pending).await {
                    Ok(()) => forced_refresh_pending = false,
                    Err(error) => error!("scheduled OSM import failed: {error:#}"),
                },
                Ok(false) => {}
                Err(error) => warn!("failed to check OSM import freshness: {error:#}"),
            }
        }
    });
}

async fn import_due(pool: &SqlitePool, config: &Config) -> Result<bool> {
    let pbf_path = config.osm_import_dir.join(PBF_FILE_NAME);
    if local_pbf_state(&pbf_path, config).await? != LocalPbfState::Fresh {
        return Ok(true);
    }

    let Some(latest) = store::latest_successful_import(pool).await? else {
        return Ok(true);
    };

    Ok(successful_import_is_stale(Some(&latest), config))
}

fn successful_import_is_stale(latest: Option<&store::OsmImportStatus>, config: &Config) -> bool {
    let Some(finished_at) = latest.and_then(|import| import.finished_at) else {
        return true;
    };

    Utc::now() - finished_at >= config.osm_import_interval
}

pub async fn run_import(pool: &SqlitePool, config: &Config, force_download: bool) -> Result<()> {
    let started_at = Utc::now();
    let import_id = store::start_import(pool, &config.osm_pbf_url, started_at).await?;
    let result = run_import_inner(pool, config, import_id, force_download).await;

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

async fn run_import_inner(
    pool: &SqlitePool,
    config: &Config,
    import_id: i64,
    force_download: bool,
) -> Result<usize> {
    info!(
        import_id,
        source_url = %config.osm_pbf_url,
        import_dir = %config.osm_import_dir.display(),
        "starting OSM import"
    );
    let pbf_path = import_pbf_path(pool, config, import_id, force_download).await?;
    info!(
        import_id,
        pbf_path = %pbf_path.display(),
        "parsing OSM PBF dump"
    );
    let points = parse_drinking_water_points(pbf_path, import_id)
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
    store::replace_water_points_for_import(pool, &points, Utc::now(), import_id).await?;
    info!(import_id, "clearing route analysis cache after OSM import");
    store::clear_route_cache(pool).await?;

    info!(
        import_id,
        water_point_count = count,
        "activated imported OSM water points"
    );
    Ok(count)
}

async fn download_pbf(url: &str, import_dir: &Path, import_id: i64) -> Result<PathBuf> {
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

    let result = download_pbf_inner(url, &final_path, &tmp_path, import_id).await;
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path).await;
    }

    result.map(|_| final_path)
}

async fn import_pbf_path(
    pool: &SqlitePool,
    config: &Config,
    import_id: i64,
    force_download: bool,
) -> Result<PathBuf> {
    let local_path = config.osm_import_dir.join(PBF_FILE_NAME);
    if !force_download
        && !store::has_water_points(pool).await?
        && local_pbf_state(&local_path, config).await? == LocalPbfState::Fresh
    {
        info!(
            pbf_path = %local_path.display(),
            "using existing OSM PBF dump for initial import"
        );
        return Ok(local_path);
    }

    download_pbf(&config.osm_pbf_url, &config.osm_import_dir, import_id).await
}

async fn local_pbf_state(path: &Path, config: &Config) -> Result<LocalPbfState> {
    let metadata = match fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LocalPbfState::Absent);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to stat {}", path.display()));
        }
    };
    if !metadata.is_file() {
        anyhow::bail!("OSM PBF path is not a regular file: {}", path.display());
    }

    let modified = metadata
        .modified()
        .with_context(|| format!("failed to read modification time for {}", path.display()))?;
    let modified_at: DateTime<Utc> = modified.into();
    let age = Utc::now() - modified_at;

    if age <= config.osm_import_interval {
        Ok(LocalPbfState::Fresh)
    } else {
        Ok(LocalPbfState::Stale)
    }
}

async fn download_pbf_inner(
    url: &str,
    final_path: &Path,
    tmp_path: &Path,
    import_id: i64,
) -> Result<()> {
    const AQUATRACE_USER_AGENT: &str = concat!("AquaTrace/", env!("CARGO_PKG_VERSION"));
    let client = reqwest::Client::builder()
        .user_agent(AQUATRACE_USER_AGENT)
        .build()
        .context("failed to build OSM download client")?;
    let mut response = client
        .get(url)
        .header(USER_AGENT, AQUATRACE_USER_AGENT)
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
    let download_started_at = Instant::now();
    let mut last_progress_log = Instant::now();

    info!(
        import_id,
        phase = "downloading_pbf",
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

        if last_progress_log.elapsed() >= StdDuration::from_secs(PROGRESS_LOG_SECONDS) {
            log_download_progress(
                import_id,
                downloaded_bytes,
                content_length,
                download_started_at.elapsed(),
            );
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
        import_id,
        phase = "downloading_pbf",
        downloaded_bytes,
        elapsed_seconds = download_started_at.elapsed().as_secs(),
        target = %final_path.display(),
        "finished OSM PBF download"
    );

    Ok(())
}

fn log_download_progress(
    import_id: i64,
    downloaded_bytes: u64,
    content_length: Option<u64>,
    elapsed: StdDuration,
) {
    match content_length {
        Some(total_bytes) if total_bytes > 0 => {
            let percent = downloaded_bytes as f64 * 100.0 / total_bytes as f64;
            info!(
                import_id,
                phase = "downloading_pbf",
                downloaded_bytes,
                total_bytes,
                percent = format_args!("{percent:.1}"),
                elapsed_seconds = elapsed.as_secs(),
                "OSM PBF download in progress"
            );
        }
        _ => {
            info!(
                import_id,
                phase = "downloading_pbf",
                downloaded_bytes,
                elapsed_seconds = elapsed.as_secs(),
                "OSM PBF download in progress"
            );
        }
    }
}

#[derive(Default)]
struct ParseProgress {
    bytes_read: AtomicU64,
    elements_processed: AtomicU64,
}

struct ProgressReader<R> {
    inner: R,
    progress: Arc<ParseProgress>,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> IoResult<usize> {
        let bytes_read = self.inner.read(buffer)?;
        self.progress
            .bytes_read
            .fetch_add(bytes_read as u64, Ordering::Relaxed);
        Ok(bytes_read)
    }
}

async fn parse_drinking_water_points(path: PathBuf, import_id: i64) -> Result<Vec<OsmWaterPoint>> {
    let total_bytes = fs::metadata(&path)
        .await
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    let progress = Arc::new(ParseProgress::default());
    let task_progress = Arc::clone(&progress);
    let task_path = path.clone();
    let mut parser_task = tokio::task::spawn_blocking(move || {
        parse_drinking_water_points_blocking(&task_path, task_progress)
    });
    let started_at = Instant::now();
    let progress_interval = StdDuration::from_secs(PROGRESS_LOG_SECONDS);
    let mut ticker = interval_at(TokioInstant::now() + progress_interval, progress_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            result = &mut parser_task => {
                let points = result.context("OSM PBF parser task failed")??;
                log_parse_progress(import_id, &progress, total_bytes, started_at.elapsed());
                return Ok(points);
            }
            _ = ticker.tick() => {
                log_parse_progress(import_id, &progress, total_bytes, started_at.elapsed());
            }
        }
    }
}

fn log_parse_progress(
    import_id: i64,
    progress: &ParseProgress,
    total_bytes: u64,
    elapsed: StdDuration,
) {
    let bytes_read = progress.bytes_read.load(Ordering::Relaxed);
    let elements_processed = progress.elements_processed.load(Ordering::Relaxed);
    let estimated_percent = estimated_read_percent(bytes_read, total_bytes);
    info!(
        import_id,
        phase = "parsing_pbf",
        bytes_read,
        total_bytes,
        estimated_percent = format_args!("{estimated_percent:.1}"),
        elements_processed,
        elapsed_seconds = elapsed.as_secs(),
        "OSM import in progress (estimated from PBF bytes read)"
    );
}

fn estimated_read_percent(bytes_read: u64, total_bytes: u64) -> f64 {
    if total_bytes == 0 {
        100.0
    } else {
        (bytes_read as f64 * 100.0 / total_bytes as f64).min(100.0)
    }
}

fn parse_drinking_water_points_blocking(
    path: &Path,
    progress: Arc<ParseProgress>,
) -> Result<Vec<OsmWaterPoint>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = ElementReader::new(ProgressReader {
        inner: file,
        progress: Arc::clone(&progress),
    });

    let points = reader
        .par_map_reduce(
            |element| {
                progress.elements_processed.fetch_add(1, Ordering::Relaxed);
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

    #[test]
    fn startup_requires_blocking_import_without_complete_local_data() {
        for (pbf_state, database_is_usable) in [
            (LocalPbfState::Absent, false),
            (LocalPbfState::Absent, true),
            (LocalPbfState::Fresh, false),
            (LocalPbfState::Stale, false),
        ] {
            assert_eq!(
                startup_action(pbf_state, database_is_usable, false, false, false),
                StartupAction::BlockingImport
            );
        }
    }

    #[test]
    fn usable_data_refreshes_in_background_when_requested_or_stale() {
        assert_eq!(
            startup_action(LocalPbfState::Fresh, true, true, false, false),
            StartupAction::BackgroundRefresh
        );
        assert_eq!(
            startup_action(LocalPbfState::Stale, true, false, true, false),
            StartupAction::BackgroundRefresh
        );
        assert_eq!(
            startup_action(LocalPbfState::Fresh, true, false, true, true),
            StartupAction::BackgroundRefresh
        );
    }

    #[test]
    fn disabling_startup_refresh_keeps_usable_data_ready() {
        assert_eq!(
            startup_action(LocalPbfState::Stale, true, false, false, true),
            StartupAction::Ready
        );
        assert_eq!(
            startup_action(LocalPbfState::Fresh, true, false, true, false),
            StartupAction::Ready
        );
    }

    #[test]
    fn estimated_read_progress_is_bounded() {
        assert_eq!(estimated_read_percent(25, 100), 25.0);
        assert_eq!(estimated_read_percent(125, 100), 100.0);
        assert_eq!(estimated_read_percent(0, 0), 100.0);
    }
}
