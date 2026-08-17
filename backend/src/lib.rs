use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{Duration, Utc};
use serde::Serialize;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

pub mod geometry;
mod gpx_archive;
pub mod gpx_parser;
pub mod hash;
pub mod osm_import;
pub mod store;
pub mod types;

use geometry::project_water_points;
use gpx_archive::{ArchiveCategory, GpxArchive, safe_filename, submission_id};
use gpx_parser::parse_gpx_route;
use hash::sha256_hex;
use types::{AnalyzeResponse, BBox};

const DEFAULT_MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_MAX_ANALYSIS_DISTANCE_M: f64 = 500.0;
const DEFAULT_GPX_LOG_DIR: &str = "data/log";
const DEFAULT_GPX_LOG_RETENTION_DAYS: i64 = 90;
const DEFAULT_GPX_LOG_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_OSM_PBF_URL: &str = "https://download.geofabrik.de/europe-latest.osm.pbf";
const DEFAULT_OSM_IMPORT_INTERVAL_SECONDS: i64 = 1_296_000;
const DEFAULT_OSM_IMPORT_DIR: &str = "data/osm";

#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    config: Arc<Config>,
    gpx_archive: GpxArchive,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub max_upload_bytes: usize,
    pub max_analysis_distance_m: f64,
    pub gpx_log_dir: PathBuf,
    pub gpx_log_retention: Duration,
    pub gpx_log_max_bytes: u64,
    pub osm_pbf_url: String,
    pub osm_import_interval: Duration,
    pub osm_import_dir: PathBuf,
    pub osm_import_on_startup: bool,
    pub route_cache_ttl: Duration,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/analyze", post(analyze))
        .layer(DefaultBodyLimit::max(state.config.max_upload_bytes))
        .route_layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn connect_database(database_url: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)
        .with_context(|| format!("invalid DATABASE_URL: {database_url}"))?
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .context("failed to connect to SQLite")?;

    run_migrations(&pool).await?;
    Ok(pool)
}

pub async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("failed to run database migrations")
}

pub fn ensure_sqlite_parent_exists(database_url: &str) -> Result<()> {
    let Some(path) = database_url.strip_prefix("sqlite:") else {
        return Ok(());
    };

    if path == ":memory:" {
        return Ok(());
    }

    let path = path.trim_start_matches("//");
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create database directory {}", parent.display()))?;
    }

    Ok(())
}

pub async fn app_state(config: Config) -> Result<AppState> {
    ensure_sqlite_parent_exists(&config.database_url)?;
    let pool = connect_database(&config.database_url).await?;

    if config.osm_import_on_startup {
        osm_import::ensure_initial_data(&pool, &config).await?;
    }

    let archive_retention = config
        .gpx_log_retention
        .to_std()
        .context("GPX_LOG_RETENTION_DAYS must be positive")?;
    let gpx_archive = GpxArchive::new(
        config.gpx_log_dir.clone(),
        archive_retention,
        config.gpx_log_max_bytes,
    );
    if let Err(error) = gpx_archive.prepare().await {
        warn!(error = %error, "failed to prepare GPX archive; uploads will continue");
    }

    let state = AppState {
        pool,
        config: Arc::new(config),
        gpx_archive,
    };
    osm_import::spawn_import_scheduler(state.pool.clone(), Arc::clone(&state.config));

    Ok(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn analyze(
    State(state): State<AppState>,
    multipart: Multipart,
) -> ApiResult<Json<AnalyzeResponse>> {
    let request_start = Instant::now();
    let upload = match read_gpx_upload(multipart, state.config.max_upload_bytes).await {
        Ok(upload) => upload,
        Err(error) => {
            warn!(
                error_code = error.code,
                elapsed_ms = request_start.elapsed().as_millis(),
                "GPX submission rejected before archival"
            );
            return Err(error);
        }
    };
    let gpx_hash = sha256_hex(&upload.bytes);
    let submission_id = submission_id(&gpx_hash);
    let filename = safe_filename(&upload.filename);
    info!(
        submission_id,
        filename,
        upload_bytes = upload.bytes.len(),
        gpx_hash,
        "received GPX submission"
    );

    let result = analyze_uploaded_gpx(&state, &upload.bytes, &gpx_hash).await;
    let (category, outcome, error_code, cache_hit) = match &result {
        Ok(success) => (
            ArchiveCategory::Valid,
            "valid",
            None,
            Some(success.cache_hit),
        ),
        Err(error) => (ArchiveCategory::Error, "error", Some(error.code), None),
    };
    match state
        .gpx_archive
        .archive(&submission_id, &upload.filename, &upload.bytes, category)
        .await
    {
        Ok(record) => info!(
            submission_id,
            archive_category = category.as_str(),
            archive_path = %record.path.display(),
            compressed_bytes = record.compressed_bytes,
            "archived GPX submission"
        ),
        Err(error) => warn!(
            submission_id,
            archive_category = category.as_str(),
            error = %error,
            "failed to archive GPX submission; preserving API response"
        ),
    }
    info!(
        submission_id,
        outcome,
        error_code,
        cache_hit,
        elapsed_ms = request_start.elapsed().as_millis(),
        "completed GPX submission"
    );

    result.map(|success| Json(success.analysis))
}

struct AnalysisSuccess {
    analysis: AnalyzeResponse,
    cache_hit: bool,
}

async fn analyze_uploaded_gpx(
    state: &AppState,
    file: &[u8],
    gpx_hash: &str,
) -> ApiResult<AnalysisSuccess> {
    let now = Utc::now();

    if let Some(cached) = store::get_route_cache(&state.pool, gpx_hash, now).await? {
        return Ok(AnalysisSuccess {
            analysis: cached,
            cache_hit: true,
        });
    }

    let parse_start = Instant::now();
    let route = parse_gpx_route(file).map_err(ApiError::bad_request)?;
    info!(
        points = route.points.len(),
        elapsed_ms = parse_start.elapsed().as_millis(),
        "parsed GPX route"
    );

    let sqlite_start = Instant::now();
    let route_bbox = BBox::from_points(&route.points)
        .ok_or_else(|| ApiError::bad_request(anyhow::anyhow!("empty route")))?;
    let osm_bbox = route_bbox.expand_meters(state.config.max_analysis_distance_m);
    let water_points = store::water_points_in_bbox(&state.pool, osm_bbox).await?;
    info!(
        water_point_count = water_points.len(),
        elapsed_ms = sqlite_start.elapsed().as_millis(),
        "loaded candidate water points"
    );

    let projection_start = Instant::now();
    let mut projected =
        project_water_points(&route, &water_points, state.config.max_analysis_distance_m);
    projected.sort_by(|a, b| a.km.partial_cmp(&b.km).unwrap_or(std::cmp::Ordering::Equal));
    info!(
        projected_water_point_count = projected.len(),
        elapsed_ms = projection_start.elapsed().as_millis(),
        "projected water points onto route"
    );

    let analysis = AnalyzeResponse {
        route,
        water_points: projected,
    };

    let cache_start = Instant::now();
    store::put_route_cache(
        &state.pool,
        gpx_hash,
        &analysis,
        now,
        state.config.route_cache_ttl,
    )
    .await?;
    info!(
        elapsed_ms = cache_start.elapsed().as_millis(),
        "wrote route analysis cache"
    );

    Ok(AnalysisSuccess {
        analysis,
        cache_hit: false,
    })
}

struct GpxUpload {
    filename: String,
    bytes: Bytes,
}

async fn read_gpx_upload(
    mut multipart: Multipart,
    max_upload_bytes: usize,
) -> ApiResult<GpxUpload> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(ApiError::bad_request)?
    {
        if field.name() != Some("file") {
            continue;
        }

        let filename = field.file_name().unwrap_or_default().to_owned();
        if !filename.to_ascii_lowercase().ends_with(".gpx") {
            return Err(ApiError::bad_request(anyhow::anyhow!(
                "uploaded file must have a .gpx extension"
            )));
        }

        let bytes = field.bytes().await.map_err(ApiError::bad_request)?;
        if bytes.len() > max_upload_bytes {
            return Err(ApiError::payload_too_large());
        }
        return Ok(GpxUpload { filename, bytes });
    }

    Err(ApiError::bad_request(anyhow::anyhow!(
        "missing multipart field named file"
    )))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:data/aquatrace.db".to_owned());
        let bind_addr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:3000".to_owned())
            .parse()
            .context("BIND_ADDR must be a socket address, for example 0.0.0.0:3000")?;
        let osm_pbf_url =
            std::env::var("OSM_PBF_URL").unwrap_or_else(|_| DEFAULT_OSM_PBF_URL.to_owned());
        let osm_import_dir = std::env::var("OSM_IMPORT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_OSM_IMPORT_DIR));
        let gpx_log_retention_days =
            parse_i64_env("GPX_LOG_RETENTION_DAYS", DEFAULT_GPX_LOG_RETENTION_DAYS)?;
        if gpx_log_retention_days <= 0 {
            anyhow::bail!("GPX_LOG_RETENTION_DAYS must be a positive integer");
        }
        let gpx_log_max_bytes = parse_u64_env("GPX_LOG_MAX_BYTES", DEFAULT_GPX_LOG_MAX_BYTES)?;
        if gpx_log_max_bytes == 0 {
            anyhow::bail!("GPX_LOG_MAX_BYTES must be a positive integer");
        }

        Ok(Self {
            bind_addr,
            database_url,
            max_upload_bytes: parse_usize_env("MAX_UPLOAD_BYTES", DEFAULT_MAX_UPLOAD_BYTES)?,
            max_analysis_distance_m: parse_f64_env(
                "MAX_ANALYSIS_DISTANCE_M",
                DEFAULT_MAX_ANALYSIS_DISTANCE_M,
            )?,
            gpx_log_dir: std::env::var("GPX_LOG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(DEFAULT_GPX_LOG_DIR)),
            gpx_log_retention: Duration::days(gpx_log_retention_days),
            gpx_log_max_bytes,
            osm_pbf_url,
            osm_import_interval: Duration::seconds(parse_i64_env(
                "OSM_IMPORT_INTERVAL_SECONDS",
                DEFAULT_OSM_IMPORT_INTERVAL_SECONDS,
            )?),
            osm_import_dir,
            osm_import_on_startup: parse_bool_env("OSM_IMPORT_ON_STARTUP", true)?,
            route_cache_ttl: Duration::seconds(parse_i64_env("ROUTE_CACHE_TTL_SECONDS", 86_400)?),
        })
    }
}

fn parse_bool_env(name: &str, default: bool) -> Result<bool> {
    std::env::var(name)
        .ok()
        .map(|value| match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(anyhow::anyhow!("{name} must be a boolean")),
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn parse_usize_env(name: &str, default: usize) -> Result<usize> {
    std::env::var(name)
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .with_context(|| format!("{name} must be a positive integer"))
        .map(|value| value.unwrap_or(default))
}

fn parse_u64_env(name: &str, default: u64) -> Result<u64> {
    std::env::var(name)
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()
        .with_context(|| format!("{name} must be a positive integer"))
        .map(|value| value.unwrap_or(default))
}

fn parse_i64_env(name: &str, default: i64) -> Result<i64> {
    std::env::var(name)
        .ok()
        .map(|value| value.parse::<i64>())
        .transpose()
        .with_context(|| format!("{name} must be a positive integer"))
        .map(|value| value.unwrap_or(default))
}

fn parse_f64_env(name: &str, default: f64) -> Result<f64> {
    std::env::var(name)
        .ok()
        .map(|value| value.parse::<f64>())
        .transpose()
        .with_context(|| format!("{name} must be a number"))
        .map(|value| value.unwrap_or(default))
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    source: Option<anyhow::Error>,
}

type ApiResult<T> = std::result::Result<T, ApiError>;

impl ApiError {
    fn bad_request(error: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            source: Some(error.into()),
        }
    }

    fn payload_too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "payload_too_large",
            source: None,
        }
    }
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_server_error",
            source: Some(error.into()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Some(source) = &self.source {
            warn!("api error {}: {:#}", self.code, source);
        }

        (self.status, Json(ErrorResponse { error: self.code })).into_response()
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: &'static str,
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Read, path::PathBuf};

    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use flate2::read::GzDecoder;
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::*;
    use crate::types::OsmWaterPoint;

    const GPX: &str = r#"<?xml version="1.0"?>
<gpx version="1.1" creator="test" xmlns="http://www.topografix.com/GPX/1/1">
  <trk><trkseg>
    <trkpt lat="45.0" lon="5.0"><ele>100</ele></trkpt>
    <trkpt lat="45.01" lon="5.0"><ele>120</ele></trkpt>
  </trkseg></trk>
</gpx>"#;

    async fn test_app(points: Vec<OsmWaterPoint>) -> (Router, TempDir) {
        let temp = TempDir::new().unwrap();
        let database_url = format!("sqlite:{}/aquatrace.db", temp.path().display());
        let config = Config {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            database_url,
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
            max_analysis_distance_m: DEFAULT_MAX_ANALYSIS_DISTANCE_M,
            gpx_log_dir: temp.path().join("log"),
            gpx_log_retention: Duration::days(DEFAULT_GPX_LOG_RETENTION_DAYS),
            gpx_log_max_bytes: DEFAULT_GPX_LOG_MAX_BYTES,
            osm_pbf_url: DEFAULT_OSM_PBF_URL.to_owned(),
            osm_import_interval: Duration::days(15),
            osm_import_dir: temp.path().join("osm"),
            osm_import_on_startup: false,
            route_cache_ttl: Duration::days(1),
        };
        let state = app_state(config).await.unwrap();
        store::replace_water_points(&state.pool, &points, Utc::now())
            .await
            .unwrap();
        (build_router(state), temp)
    }

    fn multipart_request(gpx: &str) -> Request<Body> {
        let boundary = "aquatrace-test";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"route.gpx\"\r\nContent-Type: application/gpx+xml\r\n\r\n{gpx}\r\n--{boundary}--\r\n"
        );

        Request::builder()
            .method(Method::POST)
            .uri("/api/analyze")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap()
    }

    async fn response_json(response: axum::response::Response) -> Value {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn archive_files(temp: &TempDir, category: &str) -> Vec<PathBuf> {
        fs::read_dir(temp.path().join("log").join(category))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "gz"))
            .collect()
    }

    fn decompress(path: &Path) -> Vec<u8> {
        let mut decoder = GzDecoder::new(fs::File::open(path).unwrap());
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        decoded
    }

    #[tokio::test]
    async fn analyze_route_success_and_persists_cache() {
        let (app, temp) = test_app(vec![OsmWaterPoint {
            osm_id: 123,
            lat: 45.005,
            lon: 5.001,
            name: Some("Village Fountain".to_owned()),
        }])
        .await;
        let response = app.oneshot(multipart_request(GPX)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;

        assert!(json["route"]["distance_m"].as_f64().unwrap() > 1000.0);
        assert_eq!(json["water_points"][0]["osm_id"], 123);
        assert_eq!(json["water_points"][0]["name"], "Village Fountain");
        let files = archive_files(&temp, "valid");
        assert_eq!(files.len(), 1);
        assert_eq!(decompress(&files[0]), GPX.as_bytes());
        assert!(archive_files(&temp, "error").is_empty());
    }

    #[tokio::test]
    async fn route_cache_hit_uses_cached_analysis() {
        let (app, temp) = test_app(vec![]).await;
        let first = app.clone().oneshot(multipart_request(GPX)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = app.oneshot(multipart_request(GPX)).await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(archive_files(&temp, "valid").len(), 2);
    }

    #[tokio::test]
    async fn invalid_gpx_is_rejected() {
        let (app, temp) = test_app(vec![]).await;
        let response = app.oneshot(multipart_request("not gpx")).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let files = archive_files(&temp, "error");
        assert_eq!(files.len(), 1);
        assert_eq!(decompress(&files[0]), b"not gpx");
        assert!(archive_files(&temp, "valid").is_empty());
    }

    #[tokio::test]
    async fn empty_gpx_is_rejected_and_archived() {
        let (app, temp) = test_app(vec![]).await;
        let response = app.oneshot(multipart_request("")).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let files = archive_files(&temp, "error");
        assert_eq!(files.len(), 1);
        assert!(decompress(&files[0]).is_empty());
    }

    #[tokio::test]
    async fn archive_failure_does_not_change_successful_response() {
        let (app, temp) = test_app(vec![]).await;
        let log_dir = temp.path().join("log");
        fs::remove_dir_all(&log_dir).unwrap();
        fs::write(&log_dir, "archive unavailable").unwrap();

        let response = app.oneshot(multipart_request(GPX)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
