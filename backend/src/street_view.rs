use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use reqwest::Client;
use serde::Deserialize;
use tokio::{sync::Semaphore, task::JoinSet};

const GOOGLE_METADATA_URL: &str = "https://maps.googleapis.com/maps/api/streetview/metadata";
const GOOGLE_MAPS_URL: &str = "https://www.google.com/maps/@";
const LOOKUP_TIMEOUT: Duration = Duration::from_millis(500);
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_CACHE_ENTRIES: usize = 100_000;
const MAX_PARALLEL_LOOKUPS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coordinates {
    pub lat: f64,
    pub lon: f64,
}

impl Coordinates {
    pub fn is_valid(self) -> bool {
        self.lat.is_finite()
            && self.lon.is_finite()
            && (-90.0..=90.0).contains(&self.lat)
            && (-180.0..=180.0).contains(&self.lon)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupStatus {
    Found,
    NotFound,
    Timeout,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupResult {
    pub url: Option<String>,
    pub status: LookupStatus,
}

impl LookupResult {
    fn found(url: String) -> Self {
        Self {
            url: Some(url),
            status: LookupStatus::Found,
        }
    }

    fn without_url(status: LookupStatus) -> Self {
        Self { url: None, status }
    }
}

#[derive(Clone)]
pub struct StreetViewService {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    api_key: String,
    metadata_url: String,
    client: Client,
    semaphore: Semaphore,
    cache: Mutex<HashMap<CacheKey, CachedLookup>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    lat_e6: i32,
    lon_e6: i32,
}

impl From<Coordinates> for CacheKey {
    fn from(coordinates: Coordinates) -> Self {
        Self {
            lat_e6: (coordinates.lat * 1_000_000.0).round() as i32,
            lon_e6: (coordinates.lon * 1_000_000.0).round() as i32,
        }
    }
}

#[derive(Clone)]
struct CachedLookup {
    result: LookupResult,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct MetadataResponse {
    status: String,
    location: Option<MetadataLocation>,
}

#[derive(Deserialize)]
struct MetadataLocation {
    lat: f64,
    lng: f64,
}

impl StreetViewService {
    pub fn new(api_key: Option<String>) -> Self {
        Self::with_metadata_url(api_key, GOOGLE_METADATA_URL)
    }

    fn with_metadata_url(api_key: Option<String>, metadata_url: &str) -> Self {
        let api_key = api_key.and_then(|key| {
            let key = key.trim().to_owned();
            (!key.is_empty()).then_some(key)
        });
        let inner = api_key.map(|api_key| {
            Arc::new(Inner {
                api_key,
                metadata_url: metadata_url.to_owned(),
                client: Client::builder()
                    .timeout(LOOKUP_TIMEOUT)
                    .build()
                    .expect("Street View HTTP client configuration must be valid"),
                semaphore: Semaphore::new(MAX_PARALLEL_LOOKUPS),
                cache: Mutex::new(HashMap::new()),
            })
        });

        Self { inner }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(api_key: Option<String>, metadata_url: &str) -> Self {
        Self::with_metadata_url(api_key, metadata_url)
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub async fn lookup(&self, coordinates: Coordinates) -> LookupResult {
        if !coordinates.is_valid() {
            return LookupResult::without_url(LookupStatus::Error);
        }

        let Some(inner) = &self.inner else {
            return LookupResult::without_url(LookupStatus::Error);
        };
        let cache_key = CacheKey::from(coordinates);
        if let Some(result) = cached_result(inner, cache_key) {
            return result;
        }

        let Ok(_permit) = inner.semaphore.acquire().await else {
            return LookupResult::without_url(LookupStatus::Error);
        };
        // Another lookup for the same coordinates may have completed while this one was queued.
        if let Some(result) = cached_result(inner, cache_key) {
            return result;
        }

        let location = format!("{:.6},{:.6}", coordinates.lat, coordinates.lon);
        let response = inner
            .client
            .get(&inner.metadata_url)
            .query(&[
                ("location", location.as_str()),
                ("radius", "50"),
                ("key", inner.api_key.as_str()),
            ])
            .send()
            .await;

        let result = match response {
            Ok(response) if response.status().is_success() => {
                match response.json::<MetadataResponse>().await {
                    Ok(metadata) if metadata.status == "OK" => metadata
                        .location
                        .filter(|location| {
                            Coordinates {
                                lat: location.lat,
                                lon: location.lng,
                            }
                            .is_valid()
                        })
                        .map(|location| {
                            LookupResult::found(format!(
                                "{GOOGLE_MAPS_URL}?api=1&map_action=pano&viewpoint={:.6},{:.6}",
                                location.lat, location.lng
                            ))
                        })
                        .unwrap_or_else(|| LookupResult::without_url(LookupStatus::Error)),
                    Ok(metadata) if metadata.status == "ZERO_RESULTS" => {
                        LookupResult::without_url(LookupStatus::NotFound)
                    }
                    Ok(_) | Err(_) => LookupResult::without_url(LookupStatus::Error),
                }
            }
            Ok(_) => LookupResult::without_url(LookupStatus::Error),
            Err(error) if error.is_timeout() => LookupResult::without_url(LookupStatus::Timeout),
            Err(_) => LookupResult::without_url(LookupStatus::Error),
        };

        if matches!(result.status, LookupStatus::Found | LookupStatus::NotFound) {
            let mut cache = inner
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cache.len() >= MAX_CACHE_ENTRIES {
                let now = Instant::now();
                cache.retain(|_, cached| cached.expires_at > now);
                if cache.len() >= MAX_CACHE_ENTRIES
                    && let Some(oldest_key) = cache
                        .iter()
                        .min_by_key(|(_, cached)| cached.expires_at)
                        .map(|(key, _)| *key)
                {
                    cache.remove(&oldest_key);
                }
            }
            cache.insert(
                cache_key,
                CachedLookup {
                    result: result.clone(),
                    expires_at: Instant::now() + CACHE_TTL,
                },
            );
        }

        result
    }

    pub async fn lookup_many(&self, coordinates: &[Coordinates]) -> Vec<LookupResult> {
        let mut tasks = JoinSet::new();
        for (index, coordinates) in coordinates.iter().copied().enumerate() {
            let service = self.clone();
            tasks.spawn(async move { (index, service.lookup(coordinates).await) });
        }

        let mut results = vec![LookupResult::without_url(LookupStatus::Error); coordinates.len()];
        while let Some(joined) = tasks.join_next().await {
            if let Ok((index, result)) = joined {
                results[index] = result;
            }
        }
        results
    }
}

fn cached_result(inner: &Inner, key: CacheKey) -> Option<LookupResult> {
    let mut cache = inner
        .cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match cache.get(&key) {
        Some(cached) if cached.expires_at > Instant::now() => Some(cached.result.clone()),
        Some(_) => {
            cache.remove(&key);
            None
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{Json, Router, extract::Query, routing::get};
    use serde_json::{Value, json};

    use super::*;

    fn coordinates() -> Coordinates {
        Coordinates {
            lat: 45.123456,
            lon: 5.654321,
        }
    }

    async fn start_server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/metadata")
    }

    #[tokio::test]
    async fn returns_link_for_available_panorama_and_caches_it() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let app = Router::new().route(
            "/metadata",
            get(move |Query(query): Query<HashMap<String, String>>| {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        query.get("location").map(String::as_str),
                        Some("45.123456,5.654321")
                    );
                    assert_eq!(query.get("radius").map(String::as_str), Some("50"));
                    assert_eq!(query.get("key").map(String::as_str), Some("secret"));
                    Json(json!({
                        "status": "OK",
                        "location": { "lat": 45.123400, "lng": 5.654300 }
                    }))
                }
            }),
        );
        let metadata_url = start_server(app).await;
        let service =
            StreetViewService::with_metadata_url(Some("secret".to_owned()), &metadata_url);

        let first = service.lookup(coordinates()).await;
        let second = service.lookup(coordinates()).await;

        assert_eq!(first.status, LookupStatus::Found);
        assert_eq!(
            first.url.as_deref(),
            Some(
                "https://www.google.com/maps/@?api=1&map_action=pano&viewpoint=45.123400,5.654300"
            )
        );
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn caches_confirmed_absence() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let app = Router::new().route(
            "/metadata",
            get(move || {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({ "status": "ZERO_RESULTS" }))
                }
            }),
        );
        let metadata_url = start_server(app).await;
        let service =
            StreetViewService::with_metadata_url(Some("secret".to_owned()), &metadata_url);

        assert_eq!(
            service.lookup(coordinates()).await.status,
            LookupStatus::NotFound
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            service.lookup(coordinates()).await.status,
            LookupStatus::NotFound
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn times_out_after_500_milliseconds_without_caching_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let app = Router::new().route(
            "/metadata",
            get(move || {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(650)).await;
                    Json(json!({ "status": "ZERO_RESULTS" }))
                }
            }),
        );
        let metadata_url = start_server(app).await;
        let service =
            StreetViewService::with_metadata_url(Some("secret".to_owned()), &metadata_url);

        assert_eq!(
            service.lookup(coordinates()).await.status,
            LookupStatus::Timeout
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            service
                .inner
                .as_ref()
                .unwrap()
                .cache
                .lock()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn batch_preserves_input_order() {
        let app = Router::new().route(
            "/metadata",
            get(|| async { Json::<Value>(json!({ "status": "ZERO_RESULTS" })) }),
        );
        let metadata_url = start_server(app).await;
        let service =
            StreetViewService::with_metadata_url(Some("secret".to_owned()), &metadata_url);
        let locations = [
            Coordinates {
                lat: 45.0,
                lon: 5.0,
            },
            Coordinates {
                lat: 46.0,
                lon: 6.0,
            },
        ];

        let results = service.lookup_many(&locations).await;

        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|result| result.status == LookupStatus::NotFound)
        );
    }

    #[tokio::test]
    async fn batch_limits_parallel_google_requests_to_16() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let handler_active = Arc::clone(&active);
        let handler_maximum = Arc::clone(&maximum);
        let app = Router::new().route(
            "/metadata",
            get(move || {
                let active = Arc::clone(&handler_active);
                let maximum = Arc::clone(&handler_maximum);
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Json(json!({ "status": "ZERO_RESULTS" }))
                }
            }),
        );
        let metadata_url = start_server(app).await;
        let service =
            StreetViewService::with_metadata_url(Some("secret".to_owned()), &metadata_url);
        let locations = (0..32)
            .map(|index| Coordinates {
                lat: 45.0 + f64::from(index) / 1000.0,
                lon: 5.0,
            })
            .collect::<Vec<_>>();

        let results = service.lookup_many(&locations).await;

        assert_eq!(results.len(), locations.len());
        assert!(maximum.load(Ordering::SeqCst) <= MAX_PARALLEL_LOOKUPS);
        assert!(maximum.load(Ordering::SeqCst) > 1);
    }
}
