use std::{
    collections::{HashMap, HashSet},
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
    types::{OsmElementType, OsmPlace, OsmWaterPoint, RoutePoint},
};

const IMPORT_POLL_SECONDS: u64 = 3_600;
const PROGRESS_LOG_SECONDS: u64 = 5;
const PBF_FILE_NAME: &str = "europe-latest.osm.pbf";
const DATASET_VERSION: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalPbfState {
    Absent,
    Fresh,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupAction {
    BlockingImport,
    BackgroundRefresh { force_download: bool },
    Ready,
}

fn startup_action(
    pbf_state: LocalPbfState,
    database_is_usable: bool,
    force_download: bool,
    import_on_startup: bool,
    latest_import_is_stale: bool,
    dataset_is_current: bool,
) -> StartupAction {
    if pbf_state == LocalPbfState::Absent || !database_is_usable {
        StartupAction::BlockingImport
    } else if force_download {
        StartupAction::BackgroundRefresh {
            force_download: true,
        }
    } else if !dataset_is_current
        || (import_on_startup && (pbf_state == LocalPbfState::Stale || latest_import_is_stale))
    {
        StartupAction::BackgroundRefresh {
            force_download: pbf_state == LocalPbfState::Stale,
        }
    } else {
        StartupAction::Ready
    }
}

pub async fn prepare_initial_data(
    pool: &SqlitePool,
    config: &Config,
    force_download: bool,
) -> Result<Option<bool>> {
    let pbf_path = config.osm_import_dir.join(PBF_FILE_NAME);
    let pbf_state = local_pbf_state(&pbf_path, config).await?;
    let has_water_points = store::has_water_points(pool).await?;
    let has_places = store::has_places(pool).await?;
    let latest_import = store::latest_successful_import(pool).await?;
    let database_is_usable = has_water_points && has_places && latest_import.is_some();

    let action = startup_action(
        pbf_state,
        database_is_usable,
        force_download,
        config.osm_import_on_startup,
        successful_import_is_stale(latest_import.as_ref(), config),
        successful_import_has_current_dataset(latest_import.as_ref()),
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
            Ok(None)
        }
        StartupAction::BackgroundRefresh { force_download } => {
            info!(
                pbf_state = ?pbf_state,
                force_download,
                "OSM data is usable; scheduling refresh in the background"
            );
            Ok(Some(force_download))
        }
        StartupAction::Ready => Ok(None),
    }
}

pub fn spawn_import_scheduler(
    pool: SqlitePool,
    config: Arc<Config>,
    refresh_on_start: Option<bool>,
) {
    tokio::spawn(async move {
        let mut ticker = interval(std::time::Duration::from_secs(IMPORT_POLL_SECONDS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut refresh_pending = refresh_on_start;
        ticker.tick().await;

        if let Some(force_download) = refresh_pending {
            match run_import(&pool, &config, force_download).await {
                Ok(()) => refresh_pending = None,
                Err(error) => error!("scheduled OSM import failed: {error:#}"),
            }
        }

        loop {
            ticker.tick().await;

            let import_is_due = if refresh_pending.is_some() {
                Ok(true)
            } else {
                import_due(&pool, &config).await
            };

            match import_is_due {
                Ok(true) => {
                    match run_import(&pool, &config, refresh_pending.unwrap_or(false)).await {
                        Ok(()) => refresh_pending = None,
                        Err(error) => error!("scheduled OSM import failed: {error:#}"),
                    }
                }
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

    Ok(!successful_import_has_current_dataset(Some(&latest))
        || successful_import_is_stale(Some(&latest), config))
}

fn successful_import_has_current_dataset(latest: Option<&store::OsmImportStatus>) -> bool {
    latest.is_some_and(|import| import.dataset_version == DATASET_VERSION)
}

fn successful_import_is_stale(latest: Option<&store::OsmImportStatus>, config: &Config) -> bool {
    let Some(finished_at) = latest.and_then(|import| import.finished_at) else {
        return true;
    };

    Utc::now() - finished_at >= config.osm_import_interval
}

pub async fn run_import(pool: &SqlitePool, config: &Config, force_download: bool) -> Result<()> {
    let started_at = Utc::now();
    let import_id =
        store::start_import(pool, &config.osm_pbf_url, started_at, DATASET_VERSION).await?;
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
    let pbf_path = import_pbf_path(config, import_id, force_download).await?;
    info!(
        import_id,
        pbf_path = %pbf_path.display(),
        "parsing OSM PBF dump"
    );
    let dataset = parse_osm_dataset(pbf_path, import_id)
        .await
        .context("failed to parse OSM PBF dump")?;
    let count = dataset.points.len();
    info!(
        import_id,
        water_point_count = count,
        place_count = dataset.places.len(),
        "finished parsing OSM PBF dump"
    );

    info!(
        import_id,
        water_point_count = count,
        place_count = dataset.places.len(),
        "replacing SQLite OSM dataset"
    );
    store::replace_osm_dataset_for_import(
        pool,
        &dataset.points,
        &dataset.places,
        Utc::now(),
        import_id,
    )
    .await?;
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

async fn import_pbf_path(config: &Config, import_id: i64, force_download: bool) -> Result<PathBuf> {
    let local_path = config.osm_import_dir.join(PBF_FILE_NAME);
    if !force_download && local_pbf_state(&local_path, config).await? == LocalPbfState::Fresh {
        info!(
            pbf_path = %local_path.display(),
            "using existing fresh OSM PBF dump"
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

struct ParsedOsmDataset {
    points: Vec<OsmWaterPoint>,
    places: Vec<OsmPlace>,
}

async fn parse_osm_dataset(path: PathBuf, import_id: i64) -> Result<ParsedOsmDataset> {
    let scan = run_pbf_phase(
        &path,
        import_id,
        "scanning_osm_objects",
        scan_water_objects_blocking,
    )
    .await?;
    let mut points = scan.points;
    let places = scan.places.into_values().collect();

    if scan.ways.is_empty() {
        return Ok(ParsedOsmDataset {
            points: points.into_values().collect(),
            places,
        });
    }

    let required_node_ids = Arc::new(
        scan.ways
            .values()
            .flat_map(|way| way.node_refs.iter().copied())
            .collect::<HashSet<_>>(),
    );
    let lookup_ids = Arc::clone(&required_node_ids);
    let node_coordinates = run_pbf_phase(
        &path,
        import_id,
        "resolving_way_nodes",
        move |task_path, progress| {
            resolve_node_coordinates_blocking(&task_path, progress, lookup_ids)
        },
    )
    .await?;

    let mut skipped_ways = 0_usize;
    for way in scan.ways.into_values() {
        if let Some(point) = water_point_from_way(way, &node_coordinates) {
            points.insert((point.osm_type, point.osm_id), point);
        } else {
            skipped_ways += 1;
        }
    }

    if skipped_ways > 0 {
        warn!(
            import_id,
            skipped_way_count = skipped_ways,
            "skipped eligible OSM ways with incomplete or invalid geometry"
        );
    }

    Ok(ParsedOsmDataset {
        points: points.into_values().collect(),
        places,
    })
}

async fn run_pbf_phase<T, F>(path: &Path, import_id: i64, phase: &'static str, task: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(PathBuf, Arc<ParseProgress>) -> Result<T> + Send + 'static,
{
    let total_bytes = fs::metadata(&path)
        .await
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    let progress = Arc::new(ParseProgress::default());
    let task_progress = Arc::clone(&progress);
    let task_path = path.to_path_buf();
    let mut parser_task = tokio::task::spawn_blocking(move || task(task_path, task_progress));
    let started_at = Instant::now();
    let progress_interval = StdDuration::from_secs(PROGRESS_LOG_SECONDS);
    let mut ticker = interval_at(TokioInstant::now() + progress_interval, progress_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            result = &mut parser_task => {
                let value = result.context("OSM PBF parser task failed")??;
                log_parse_progress(import_id, phase, &progress, total_bytes, started_at.elapsed());
                return Ok(value);
            }
            _ = ticker.tick() => {
                log_parse_progress(import_id, phase, &progress, total_bytes, started_at.elapsed());
            }
        }
    }
}

fn log_parse_progress(
    import_id: i64,
    phase: &str,
    progress: &ParseProgress,
    total_bytes: u64,
    elapsed: StdDuration,
) {
    let bytes_read = progress.bytes_read.load(Ordering::Relaxed);
    let elements_processed = progress.elements_processed.load(Ordering::Relaxed);
    let estimated_percent = estimated_read_percent(bytes_read, total_bytes);
    info!(
        import_id,
        phase,
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

#[derive(Default)]
struct WaterObjectScan {
    points: HashMap<(OsmElementType, i64), OsmWaterPoint>,
    places: HashMap<i64, OsmPlace>,
    ways: HashMap<i64, WaterWay>,
}

struct WaterWay {
    osm_id: i64,
    name: Option<String>,
    node_refs: Vec<i64>,
}

fn scan_water_objects_blocking(
    path: PathBuf,
    progress: Arc<ParseProgress>,
) -> Result<WaterObjectScan> {
    let file = File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = ElementReader::new(ProgressReader {
        inner: file,
        progress: Arc::clone(&progress),
    });

    reader
        .par_map_reduce(
            |element| {
                progress.elements_processed.fetch_add(1, Ordering::Relaxed);
                let mut scan = WaterObjectScan::default();
                match element {
                    Element::Node(node) => {
                        let point = RoutePoint {
                            lat: node.lat(),
                            lon: node.lon(),
                            ele: None,
                        };
                        if let Some(point) = water_point_from_parts(node.id(), point, node.tags()) {
                            scan.points.insert((point.osm_type, point.osm_id), point);
                        }
                        if let Some(place) = place_from_parts(node.id(), point, node.tags()) {
                            scan.places.insert(place.osm_id, place);
                        }
                    }
                    Element::DenseNode(node) => {
                        let point = RoutePoint {
                            lat: node.lat(),
                            lon: node.lon(),
                            ele: None,
                        };
                        if let Some(point) = water_point_from_parts(node.id(), point, node.tags()) {
                            scan.points.insert((point.osm_type, point.osm_id), point);
                        }
                        if let Some(place) = place_from_parts(node.id(), point, node.tags()) {
                            scan.places.insert(place.osm_id, place);
                        }
                    }
                    Element::Way(way) => {
                        if let Some(name) = qualifying_water_name(way.tags()) {
                            scan.ways.insert(
                                way.id(),
                                WaterWay {
                                    osm_id: way.id(),
                                    name,
                                    node_refs: way.refs().collect(),
                                },
                            );
                        }
                    }
                    Element::Relation(_) => {}
                }
                scan
            },
            WaterObjectScan::default,
            |mut left, right| {
                left.points.extend(right.points);
                left.places.extend(right.places);
                left.ways.extend(right.ways);
                left
            },
        )
        .with_context(|| format!("failed to read {}", path.display()))
}

fn resolve_node_coordinates_blocking(
    path: &Path,
    progress: Arc<ParseProgress>,
    required_node_ids: Arc<HashSet<i64>>,
) -> Result<HashMap<i64, RoutePoint>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = ElementReader::new(ProgressReader {
        inner: file,
        progress: Arc::clone(&progress),
    });

    reader
        .par_map_reduce(
            |element| {
                progress.elements_processed.fetch_add(1, Ordering::Relaxed);
                let mut coordinates = HashMap::new();
                match element {
                    Element::Node(node) if required_node_ids.contains(&node.id()) => {
                        coordinates.insert(
                            node.id(),
                            RoutePoint {
                                lat: node.lat(),
                                lon: node.lon(),
                                ele: None,
                            },
                        );
                    }
                    Element::DenseNode(node) if required_node_ids.contains(&node.id()) => {
                        coordinates.insert(
                            node.id(),
                            RoutePoint {
                                lat: node.lat(),
                                lon: node.lon(),
                                ele: None,
                            },
                        );
                    }
                    Element::Node(_)
                    | Element::DenseNode(_)
                    | Element::Way(_)
                    | Element::Relation(_) => {}
                }
                coordinates
            },
            HashMap::new,
            |mut left, right| {
                left.extend(right);
                left
            },
        )
        .with_context(|| format!("failed to read {}", path.display()))
}

fn water_point_from_parts<'a>(
    osm_id: i64,
    point: RoutePoint,
    tags: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<OsmWaterPoint> {
    let name = qualifying_water_name(tags)?;

    Some(OsmWaterPoint {
        osm_type: OsmElementType::Node,
        osm_id,
        lat: point.lat,
        lon: point.lon,
        name,
    })
}

fn place_from_parts<'a>(
    osm_id: i64,
    point: RoutePoint,
    tags: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<OsmPlace> {
    let mut place_type = None;
    let mut name = None;
    let mut aliases = Vec::new();
    for (key, value) in tags {
        match key {
            "place" if matches!(value, "city" | "town" | "village" | "hamlet") => {
                place_type = Some(value.to_owned());
            }
            "name" => name = Some(value.trim().to_owned()),
            "alt_name" | "official_name" | "name:en" | "name:fr" if !value.trim().is_empty() => {
                aliases.push(value.trim().to_owned());
            }
            _ => {}
        }
    }
    let name = name.filter(|name| !name.is_empty())?;
    let place_type = place_type?;
    let search_text = std::iter::once(name.clone())
        .chain(aliases)
        .collect::<Vec<_>>()
        .join(" ");
    Some(OsmPlace {
        osm_id,
        name,
        place_type,
        lat: point.lat,
        lon: point.lon,
        search_text,
    })
}

fn qualifying_water_name<'a>(
    tags: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<Option<String>> {
    let mut amenity = None;
    let mut drinking_water = None;
    let mut name = None;

    for (key, value) in tags {
        match key {
            "amenity" => amenity = Some(value),
            "drinking_water" => drinking_water = Some(value),
            "name" => name = Some(value.to_owned()),
            _ => {}
        }
    }

    (amenity == Some("drinking_water")
        || (amenity == Some("toilets") && drinking_water == Some("yes")))
    .then_some(name)
}

fn water_point_from_way(
    way: WaterWay,
    node_coordinates: &HashMap<i64, RoutePoint>,
) -> Option<OsmWaterPoint> {
    let closed = way.node_refs.first() == way.node_refs.last();
    let mut points = way
        .node_refs
        .iter()
        .map(|node_id| node_coordinates.get(node_id).copied())
        .collect::<Option<Vec<_>>>()?;

    if closed {
        points.pop();
    }
    if points.len() < 2 {
        return None;
    }

    let center = if closed {
        polygon_centroid(&points).unwrap_or_else(|| average_point(&points))
    } else {
        average_point(&points)
    };

    Some(OsmWaterPoint {
        osm_type: OsmElementType::Way,
        osm_id: way.osm_id,
        lat: center.lat,
        lon: center.lon,
        name: way.name,
    })
}

fn polygon_centroid(points: &[RoutePoint]) -> Option<RoutePoint> {
    if points.len() < 3 {
        return None;
    }

    let mut area_twice = 0.0;
    let mut weighted_lon = 0.0;
    let mut weighted_lat = 0.0;
    let origin_lon = points[0].lon;
    let origin_lat = points[0].lat;
    for index in 0..points.len() {
        let current = points[index];
        let next = points[(index + 1) % points.len()];
        let current_lon = current.lon - origin_lon;
        let current_lat = current.lat - origin_lat;
        let next_lon = next.lon - origin_lon;
        let next_lat = next.lat - origin_lat;
        let cross = current_lon * next_lat - next_lon * current_lat;
        area_twice += cross;
        weighted_lon += (current_lon + next_lon) * cross;
        weighted_lat += (current_lat + next_lat) * cross;
    }

    if area_twice.abs() < 1e-12 {
        return None;
    }

    Some(RoutePoint {
        lat: origin_lat + weighted_lat / (3.0 * area_twice),
        lon: origin_lon + weighted_lon / (3.0 * area_twice),
        ele: None,
    })
}

fn average_point(points: &[RoutePoint]) -> RoutePoint {
    let count = points.len() as f64;
    RoutePoint {
        lat: points.iter().map(|point| point.lat).sum::<f64>() / count,
        lon: points.iter().map(|point| point.lon).sum::<f64>() / count,
        ele: None,
    }
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
        assert_eq!(point.osm_type, OsmElementType::Node);
        assert_eq!(point.name.as_deref(), Some("Village Tap"));
    }

    #[test]
    fn extracts_named_place_and_search_aliases() {
        let place = place_from_parts(
            7,
            RoutePoint {
                lat: 45.0,
                lon: 5.0,
                ele: None,
            },
            [
                ("place", "village"),
                ("name", "Saint-Pierre"),
                ("alt_name", "St Pierre"),
            ],
        )
        .unwrap();
        assert_eq!(place.place_type, "village");
        assert_eq!(place.name, "Saint-Pierre");
        assert!(place.search_text.contains("St Pierre"));
    }

    #[test]
    fn ignores_unsupported_or_unnamed_places() {
        let point = RoutePoint {
            lat: 45.0,
            lon: 5.0,
            ele: None,
        };
        assert!(place_from_parts(1, point, [("place", "suburb"), ("name", "Centre")]).is_none());
        assert!(place_from_parts(2, point, [("place", "town")]).is_none());
    }

    #[test]
    fn extracts_toilets_with_drinking_water() {
        let point = water_point_from_parts(
            13,
            RoutePoint {
                lat: 45.0,
                lon: 5.0,
                ele: None,
            },
            [
                ("amenity", "toilets"),
                ("drinking_water", "yes"),
                ("name", "Public toilets"),
            ],
        )
        .unwrap();

        assert_eq!(point.osm_id, 13);
        assert_eq!(point.name.as_deref(), Some("Public toilets"));
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
    fn ignores_toilets_without_confirmed_drinking_water() {
        for value in ["no", "separate", "customers"] {
            assert!(
                qualifying_water_name([("amenity", "toilets"), ("drinking_water", value)])
                    .is_none()
            );
        }
        assert!(qualifying_water_name([("amenity", "toilets")]).is_none());
    }

    #[test]
    fn locates_closed_water_way_at_polygon_centroid() {
        let way = WaterWay {
            osm_id: 99,
            name: Some("Water toilets".to_owned()),
            node_refs: vec![1, 2, 3, 4, 1],
        };
        let coordinates = HashMap::from([
            (
                1,
                RoutePoint {
                    lat: 45.0,
                    lon: 5.0,
                    ele: None,
                },
            ),
            (
                2,
                RoutePoint {
                    lat: 45.0,
                    lon: 5.002,
                    ele: None,
                },
            ),
            (
                3,
                RoutePoint {
                    lat: 45.002,
                    lon: 5.002,
                    ele: None,
                },
            ),
            (
                4,
                RoutePoint {
                    lat: 45.002,
                    lon: 5.0,
                    ele: None,
                },
            ),
        ]);

        let point = water_point_from_way(way, &coordinates).unwrap();
        assert_eq!(point.osm_type, OsmElementType::Way);
        assert!((point.lat - 45.001).abs() < 1e-8);
        assert!((point.lon - 5.001).abs() < 1e-8);
    }

    #[test]
    fn rejects_water_way_with_missing_node() {
        let way = WaterWay {
            osm_id: 99,
            name: None,
            node_refs: vec![1, 2, 3, 1],
        };
        let coordinates = HashMap::from([
            (
                1,
                RoutePoint {
                    lat: 45.0,
                    lon: 5.0,
                    ele: None,
                },
            ),
            (
                2,
                RoutePoint {
                    lat: 45.0,
                    lon: 5.001,
                    ele: None,
                },
            ),
        ]);

        assert!(water_point_from_way(way, &coordinates).is_none());
    }

    #[test]
    fn locates_open_water_way_at_average_point() {
        let way = WaterWay {
            osm_id: 100,
            name: None,
            node_refs: vec![1, 2, 3],
        };
        let coordinates = HashMap::from([
            (
                1,
                RoutePoint {
                    lat: 45.0,
                    lon: 5.0,
                    ele: None,
                },
            ),
            (
                2,
                RoutePoint {
                    lat: 45.003,
                    lon: 5.003,
                    ele: None,
                },
            ),
            (
                3,
                RoutePoint {
                    lat: 45.006,
                    lon: 5.006,
                    ele: None,
                },
            ),
        ]);

        let point = water_point_from_way(way, &coordinates).unwrap();
        assert!((point.lat - 45.003).abs() < 1e-10);
        assert!((point.lon - 5.003).abs() < 1e-10);
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
                startup_action(pbf_state, database_is_usable, false, false, false, true),
                StartupAction::BlockingImport
            );
        }
    }

    #[test]
    fn usable_data_refreshes_in_background_when_requested_or_stale() {
        assert_eq!(
            startup_action(LocalPbfState::Fresh, true, true, false, false, true),
            StartupAction::BackgroundRefresh {
                force_download: true
            }
        );
        assert_eq!(
            startup_action(LocalPbfState::Stale, true, false, true, false, true),
            StartupAction::BackgroundRefresh {
                force_download: true
            }
        );
        assert_eq!(
            startup_action(LocalPbfState::Fresh, true, false, true, true, true),
            StartupAction::BackgroundRefresh {
                force_download: false
            }
        );
    }

    #[test]
    fn outdated_dataset_refreshes_from_fresh_local_pbf() {
        assert_eq!(
            startup_action(LocalPbfState::Fresh, true, false, false, false, false),
            StartupAction::BackgroundRefresh {
                force_download: false
            }
        );
    }

    #[test]
    fn disabling_startup_refresh_keeps_usable_data_ready() {
        assert_eq!(
            startup_action(LocalPbfState::Stale, true, false, false, true, true),
            StartupAction::Ready
        );
        assert_eq!(
            startup_action(LocalPbfState::Fresh, true, false, true, false, true),
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
