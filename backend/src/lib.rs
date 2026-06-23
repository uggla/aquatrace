use std::{net::SocketAddr, path::Path, str::FromStr, sync::Arc};

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
use tracing::warn;

pub mod geometry;
pub mod gpx_parser;
pub mod hash;
pub mod overpass;
pub mod store;
pub mod types;

use geometry::project_water_points;
use gpx_parser::parse_gpx_route;
use hash::sha256_hex;
use overpass::OverpassClient;
use types::{AnalyzeResponse, BBox};

const DEFAULT_MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_MAX_ANALYSIS_DISTANCE_M: f64 = 500.0;

#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    config: Arc<Config>,
    overpass: OverpassClient,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub overpass_url: String,
    pub max_upload_bytes: usize,
    pub max_analysis_distance_m: f64,
    pub route_cache_ttl: Duration,
    pub osm_cache_ttl: Duration,
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
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }
    }

    Ok(())
}

pub async fn app_state(config: Config) -> Result<AppState> {
    ensure_sqlite_parent_exists(&config.database_url)?;
    let pool = connect_database(&config.database_url).await?;
    let overpass = OverpassClient::new(config.overpass_url.clone());

    Ok(AppState {
        pool,
        config: Arc::new(config),
        overpass,
    })
}

async fn health() -> &'static str {
    "ok"
}

async fn analyze(
    State(state): State<AppState>,
    multipart: Multipart,
) -> ApiResult<Json<AnalyzeResponse>> {
    let file = read_gpx_upload(multipart, state.config.max_upload_bytes).await?;
    let gpx_hash = sha256_hex(&file);
    let now = Utc::now();

    if let Some(cached) = store::get_route_cache(&state.pool, &gpx_hash, now).await? {
        return Ok(Json(cached));
    }

    let route = parse_gpx_route(&file).map_err(ApiError::bad_request)?;
    let route_bbox = BBox::from_points(&route.points)
        .ok_or_else(|| ApiError::bad_request(anyhow::anyhow!("empty route")))?;
    let osm_bbox = route_bbox.expand_meters(state.config.max_analysis_distance_m);

    let cached_coverage = store::has_fresh_coverage(&state.pool, osm_bbox, now).await?;
    let water_points = if cached_coverage {
        store::water_points_in_bbox(&state.pool, osm_bbox).await?
    } else {
        let points = state.overpass.fetch_drinking_water(osm_bbox).await?;
        store::refresh_water_points(
            &state.pool,
            osm_bbox,
            &points,
            now,
            state.config.osm_cache_ttl,
        )
        .await?;
        points
    };

    let mut projected =
        project_water_points(&route, &water_points, state.config.max_analysis_distance_m);
    projected.sort_by(|a, b| a.km.partial_cmp(&b.km).unwrap_or(std::cmp::Ordering::Equal));

    let analysis = AnalyzeResponse {
        route,
        water_points: projected,
    };

    store::put_route_cache(
        &state.pool,
        &gpx_hash,
        &analysis,
        now,
        state.config.route_cache_ttl,
    )
    .await?;

    Ok(Json(analysis))
}

async fn read_gpx_upload(mut multipart: Multipart, max_upload_bytes: usize) -> ApiResult<Bytes> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(ApiError::bad_request)?
    {
        if field.name() != Some("file") {
            continue;
        }

        let filename = field.file_name().unwrap_or_default().to_ascii_lowercase();
        if !filename.ends_with(".gpx") {
            return Err(ApiError::bad_request(anyhow::anyhow!(
                "uploaded file must have a .gpx extension"
            )));
        }

        let bytes = field.bytes().await.map_err(ApiError::bad_request)?;
        if bytes.len() > max_upload_bytes {
            return Err(ApiError::payload_too_large());
        }
        if bytes.is_empty() {
            return Err(ApiError::bad_request(anyhow::anyhow!(
                "uploaded file is empty"
            )));
        }

        return Ok(bytes);
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
        let overpass_url = std::env::var("OVERPASS_URL")
            .unwrap_or_else(|_| "https://overpass-api.de/api/interpreter".to_owned());

        Ok(Self {
            bind_addr,
            database_url,
            overpass_url,
            max_upload_bytes: parse_usize_env("MAX_UPLOAD_BYTES", DEFAULT_MAX_UPLOAD_BYTES)?,
            max_analysis_distance_m: parse_f64_env(
                "MAX_ANALYSIS_DISTANCE_M",
                DEFAULT_MAX_ANALYSIS_DISTANCE_M,
            )?,
            route_cache_ttl: Duration::seconds(parse_i64_env("ROUTE_CACHE_TTL_SECONDS", 86_400)?),
            osm_cache_ttl: Duration::seconds(parse_i64_env("OSM_CACHE_TTL_SECONDS", 2_592_000)?),
        })
    }
}

fn parse_usize_env(name: &str, default: usize) -> Result<usize> {
    std::env::var(name)
        .ok()
        .map(|value| value.parse::<usize>())
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
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    const GPX: &str = r#"<?xml version="1.0"?>
<gpx version="1.1" creator="test" xmlns="http://www.topografix.com/GPX/1/1">
  <trk><trkseg>
    <trkpt lat="45.0" lon="5.0"><ele>100</ele></trkpt>
    <trkpt lat="45.01" lon="5.0"><ele>120</ele></trkpt>
  </trkseg></trk>
</gpx>"#;

    async fn test_app(overpass_url: String) -> (Router, TempDir) {
        let temp = TempDir::new().unwrap();
        let database_url = format!("sqlite:{}/aquatrace.db", temp.path().display());
        let config = Config {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            database_url,
            overpass_url,
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
            max_analysis_distance_m: DEFAULT_MAX_ANALYSIS_DISTANCE_M,
            route_cache_ttl: Duration::days(1),
            osm_cache_ttl: Duration::days(30),
        };
        let state = app_state(config).await.unwrap();
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

    #[tokio::test]
    async fn analyze_route_success_and_persists_cache() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/interpreter"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "elements": [
                    {"type":"node","id":123,"lat":45.005,"lon":5.001,"tags":{"amenity":"drinking_water","name":"Village Fountain"}}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (app, _temp) = test_app(format!("{}/api/interpreter", server.uri())).await;
        let response = app.oneshot(multipart_request(GPX)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;

        assert!(json["route"]["distance_m"].as_f64().unwrap() > 1000.0);
        assert_eq!(json["water_points"][0]["osm_id"], 123);
        assert_eq!(json["water_points"][0]["name"], "Village Fountain");
    }

    #[tokio::test]
    async fn route_cache_hit_avoids_overpass() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "elements": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (app, _temp) = test_app(server.uri()).await;
        let first = app.clone().oneshot(multipart_request(GPX)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = app.oneshot(multipart_request(GPX)).await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_gpx_is_rejected() {
        let server = MockServer::start().await;
        let (app, _temp) = test_app(server.uri()).await;
        let response = app.oneshot(multipart_request("not gpx")).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
