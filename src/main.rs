use anyhow::Error;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
    response::IntoResponse,
    routing::{delete, get, post},
};
use chrono::{DateTime, Utc};
use futures::future::{BoxFuture, FutureExt, Shared};
use moka::future::Cache;
use rmp_serde::{decode, encode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use std::{
    convert::Infallible,
    net::{IpAddr, Ipv6Addr},
};
use tokio::sync::Mutex;

// ------------------ Moka TTL Expiry Section ------------------

pub type CacheValue = (Option<DateTime<Utc>>, Vec<u8>);

pub struct ExpiryPolicyForCacheExt;

impl moka::Expiry<String, CacheValue> for ExpiryPolicyForCacheExt {
    fn expire_after_read(
        &self,
        _key: &String,
        _value: &CacheValue,
        _read_at: Instant,
        duration_until_expiry: Option<Duration>,
        _last_modified_at: Instant,
    ) -> Option<Duration> {
        tracing::debug!("Key to expire on read {} {:?}", _key, duration_until_expiry);
        duration_until_expiry
    }

    fn expire_after_create(
        &self,
        _key: &String,
        value: &CacheValue,
        _current_time: Instant,
    ) -> Option<Duration> {
        tracing::debug!("Key to expire on create {} {:?}", _key, value.0);

        // Return the TTL provided in the cache value.
        value.0.and_then(|expires_at| {
            let now = Utc::now();
            let chrono_duration = expires_at - now;
            if chrono_duration > chrono::Duration::zero() {
                // Try to convert to std::time::Duration, which only works for positive durations.
                chrono_duration.to_std().ok()
            } else {
                // Already expired
                None
            }
        })
    }

    fn expire_after_update(
        &self,
        _key: &String,
        _value: &CacheValue,
        _updated_at: Instant,
        duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        tracing::debug!(
            "Key to expire on update {} {:?}",
            _key,
            duration_until_expiry
        );

        duration_until_expiry
    }
}

fn calculate_deadline(ttl: Option<Duration>) -> Option<DateTime<Utc>> {
    ttl.map(|ttl| Utc::now() + chrono::Duration::from_std(ttl).unwrap())
}

pub trait CacheExt<K, V> {
    fn insert_with_ttl(&self, key: K, value: V, ttl: Option<DateTime<Utc>>) -> BoxFuture<'_, ()>;
}

impl CacheExt<String, Vec<u8>> for Cache<String, CacheValue> {
    fn insert_with_ttl(
        &self,
        key: String,
        value: Vec<u8>,
        ttl: Option<DateTime<Utc>>,
    ) -> BoxFuture<'_, ()> {
        async move {
            self.invalidate(&key).await;
            self.insert(key, (ttl, value)).await;
        }
        .boxed()
    }
}

// ------------------ SingleFlight Implementation ------------------

type SingleFlightValue<T, E> =
    Arc<Mutex<HashMap<String, Shared<BoxFuture<'static, Result<T, E>>>>>>;

// now SingleFlight has two type params: T for success, E for error
#[derive(Clone)]
struct SingleFlight<T, E> {
    inner: SingleFlightValue<T, E>,
}

impl<T, E> SingleFlight<T, E>
where
    T: Send + Sync + 'static + Clone,
    E: Send + Sync + 'static + Clone,
{
    pub fn new() -> Self {
        SingleFlight {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `f` must produce a `BoxFuture<'static, Result<T, E>>`
    pub async fn do_call<F>(&self, key: String, f: F) -> Result<T, E>
    where
        F: FnOnce() -> BoxFuture<'static, Result<T, E>>,
    {
        // lock and check if we already have an in‐flight future
        let mut guard = self.inner.lock().await;
        if let Some(shared_fut) = guard.get(&key) {
            return shared_fut.clone().await;
        }

        // first caller: create, share, and insert the future
        let shared = f().boxed().shared();
        guard.insert(key.clone(), shared.clone());
        drop(guard);

        // await the result (will be cloned for everyone who awaits)
        let result = shared.await;

        // remove it so next time we fetch again
        let mut guard = self.inner.lock().await;
        guard.remove(&key);

        result
    }
}

// ------------------ Custom Errors ------------------

#[derive(Debug, Clone, thiserror::Error)]
pub enum ApplicationError {
    #[error("Key not found: {0}")]
    KeyNotFound(String),
    #[error("Unknown error")]
    Unknown,
}

// ------------------ Application State ------------------

#[derive(Clone)]
struct AppState {
    cache: Cache<String, CacheValue>,
    singleflight: SingleFlight<Vec<u8>, ApplicationError>,
}

// ------------------ Request Payloads ------------------

#[derive(Debug, Deserialize)]
struct SetPayload {
    value: serde_json::Value,
    ttl: Option<u64>, // TTL in seconds, optional.
}

// ------------------ Handlers ------------------

#[derive(Deserialize)]
struct LoadCacheEntriesQueryParams {
    prefix: String,
}

#[derive(Serialize)]
struct LoadCacheEntriesResult {
    key: String,
    ttl: Option<DateTime<Utc>>,
}

async fn load_cache_entries(
    State(state): State<AppState>,
    Query(query_params): Query<LoadCacheEntriesQueryParams>,
) -> Result<impl IntoResponse, Infallible> {
    let entries: Vec<LoadCacheEntriesResult> = state
        .cache
        .iter()
        .filter(|(key, (_ttl, _value))| key.starts_with(&query_params.prefix))
        .map(|(key, (ttl, _value))| LoadCacheEntriesResult {
            key: key.to_string(),
            ttl,
        })
        .collect();

    let number_of_entries = entries.len();

    Ok((
        StatusCode::OK,
        Json(json!({ "count": number_of_entries, "entries": entries })),
    ))
}

async fn find_cache_by_key(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let key_clone = key.clone();

    // If not present in cache, use singleflight to deduplicate concurrent fetches.
    match state
        .singleflight
        .do_call(key.clone(), || {
            async move {
                // Try to get value from cache.
                match state.cache.get(&key).await {
                    // Since moka handles expiration via the custom expiry policy,
                    // if the item is present then it is valid.
                    Some((_, cached_value)) => Ok(cached_value),
                    None => Err(ApplicationError::KeyNotFound(key)),
                }
            }
            .boxed()
        })
        .await
    {
        Ok(cached_value) => {
            let deserialized = decode::from_slice::<serde_json::Value>(&cached_value).unwrap();

            Ok((
                StatusCode::OK,
                Json(json!({ "key": key_clone, "value": deserialized })),
            ))
        }
        Err(error) => {
            tracing::debug!("Failed to find cache by key {:?}", error);

            match error {
                ApplicationError::KeyNotFound(_) => Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({ "message": "Key not found", "error_code": "KEY_NOT_FOUND" })),
                )),
                _ => Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({ "message": "Unknown error occurred", "error_code": "UNKNOWN_ERROR" }),
                    ),
                )),
            }
        }
    }
}

async fn save_cache(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(payload): Json<SetPayload>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let ttl = payload.ttl.map(Duration::from_millis);
    let serialized = encode::to_vec(&payload.value).unwrap();

    state
        .cache
        .insert_with_ttl(key.clone(), serialized, calculate_deadline(ttl))
        .await;
    let msg = if let Some(ttl_val) = ttl {
        format!("Key {} set successfully with TTL {:?}", key, ttl_val)
    } else {
        format!("Key {} set successfully with no expiration", key)
    };
    Ok((StatusCode::OK, Json(json!({ "message": msg }))))
}

async fn delete_cache(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse, Infallible> {
    state.cache.invalidate(&key).await;
    Ok((
        StatusCode::OK,
        Json(json!({ "message": format!("Cache key {} deleted.", key) })),
    ))
}

async fn invalidate_by_text(
    State(state): State<AppState>,
    Path(text): Path<String>,
) -> Result<impl IntoResponse, Infallible> {
    let _ = state
        .cache
        .invalidate_entries_if(move |key, _cache| key.contains(&text));

    Ok((
        StatusCode::OK,
        Json(json!({ "message": format!("Cache keys invalidated") })),
    ))
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "message": "ok" })))
}

async fn fallback(uri: Uri) -> impl IntoResponse {
    tracing::error!("No route for {}", uri);
    (
        StatusCode::NOT_FOUND,
        Json(
            json!({ "message": format!("No route for {}", uri), "error_code": "ROUTE_NOT_FOUND" }),
        ),
    )
}

async fn load_cache_snapshot(
    snapshot_path: String,
    cache: &Cache<String, CacheValue>,
) -> Result<(), Error> {
    use tokio::fs;

    if let Ok(data) = fs::read(snapshot_path).await {
        let entries: Vec<(String, CacheValue)> = decode::from_slice(&data)?;
        for (key, (ttl, value)) in entries {
            cache.insert(key, (ttl, value)).await;
        }
    }

    Ok(())
}

async fn save_cache_snapshot(
    snapshot_path: String,
    cache: &Cache<String, CacheValue>,
) -> Result<(), Error> {
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;

    let mut file = File::create(snapshot_path).await?;
    let entries: Vec<(String, CacheValue)> = cache
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();

    let encoded = encode::to_vec(&entries)?;
    file.write_all(&encoded).await?;
    Ok(())
}

// ------------------ Main ------------------

pub fn mb_to_bytes(mb: u64) -> u64 {
    mb * 1024 * 1024
}

fn build_router(app_state: AppState) -> Router {
    Router::new()
        .route("/cache", get(load_cache_entries))
        .route("/cache/{key}", get(find_cache_by_key))
        .route("/cache/{key}", post(save_cache))
        .route("/cache/{key}", delete(delete_cache))
        .route("/cache/patterns/{text}", delete(invalidate_by_text))
        .route("/health-check", get(health_check))
        .fallback(fallback)
        .with_state(app_state)
}

#[tokio::main]
async fn main() {
    // Initialize tracing subscriber (logging)
    tracing_subscriber::fmt::init();

    let port = std::env::var("PORT").unwrap_or("8080".to_string());
    let snapshot_path =
        std::env::var("SNAPSHOT_LOCATION").unwrap_or("cache-snapshot.rmp".to_string());

    // Create a `Cache<u32, (Expiration, String)>` with an expiry `MyExpiry` and
    // eviction listener.
    let expiry = ExpiryPolicyForCacheExt;

    let eviction_listener = |key, _value, cause| {
        tracing::debug!("Evicted key {key}. Cause: {cause:?}");
    };

    // Create Moka cache with custom expiration policy.
    let cache: Cache<String, CacheValue> = Cache::builder()
        .weigher(
            |_k: &String, (_ttl, value): &(Option<DateTime<Utc>>, Vec<u8>)| -> u32 {
                value.len().try_into().unwrap_or(u32::MAX)
            },
        )
        .max_capacity(mb_to_bytes(2_048))
        .expire_after(expiry)
        .eviction_listener(eviction_listener)
        .build();

    // Load from disk before accepting requests.
    if let Err(e) = load_cache_snapshot(snapshot_path.clone(), &cache).await {
        tracing::warn!("Failed to load cache snapshot: {:?}", e);
    }

    let app_state = AppState {
        cache: cache.clone(),
        singleflight: SingleFlight::new(),
    };

    // Build the Axum router.
    let app = build_router(app_state);

    // Run server on localhost:<port>.
    let address = SocketAddr::from((IpAddr::from(Ipv6Addr::UNSPECIFIED), port.parse().unwrap()));
    let listener = tokio::net::TcpListener::bind(address).await.unwrap();

    tracing::info!("Starting server on {}", address);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(ShutdownInput {
            snapshot_path,
            cache,
        }))
        .await
        .unwrap();
}

struct ShutdownInput {
    snapshot_path: String,
    cache: Cache<String, CacheValue>,
}

async fn shutdown_signal(input: ShutdownInput) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Closing all remaining connections after CTRL+C");
            let _ = save_cache_snapshot(input.snapshot_path, &input.cache).await;
        },
        _ = terminate => {
            tracing::info!("Closing all remaining connections after SIGTERM");
            let _ = save_cache_snapshot(input.snapshot_path, &input.cache).await;
        },
    }

    tracing::info!("signal received, starting graceful shutdown");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use moka::future::Cache;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_snapshot_save_and_load() {
        // Create a temporary file for the snapshot
        let tmp = NamedTempFile::new().unwrap();
        let snapshot_path = tmp.path().to_str().unwrap().to_string();

        // Create a cache and insert some data
        let cache: Cache<String, CacheValue> = Cache::builder().max_capacity(10).build();
        let key = "mykey".to_string();
        let value = vec![1, 2, 3, 4];
        let expires_at = Some(Utc::now() + ChronoDuration::seconds(5));
        cache.insert(key.clone(), (expires_at, value.clone())).await;

        // Save the cache to the snapshot file
        save_cache_snapshot(snapshot_path.clone(), &cache)
            .await
            .unwrap();

        // Create a new cache and load from the snapshot
        let loaded_cache: Cache<String, CacheValue> = Cache::builder().max_capacity(10).build();
        load_cache_snapshot(snapshot_path, &loaded_cache)
            .await
            .unwrap();

        // The value should be present in the loaded cache
        let loaded = loaded_cache.get(&key).await;
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().1, value);
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;
    use axum::Router;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt; // for `collect`
    use serde_json::json;
    use tower::ServiceExt; // for `call`, `oneshot`, and `ready`

    // Helper to build the app for tests (without snapshotting)
    fn app_for_test() -> Router {
        let cache: Cache<String, CacheValue> = Cache::builder().max_capacity(100).build();
        let app_state = AppState {
            cache,
            singleflight: SingleFlight::new(),
        };
        build_router(app_state)
    }

    #[tokio::test]
    async fn test_health_check() {
        let app = app_for_test();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health-check")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_set_and_get_cache_value() {
        let app = app_for_test();

        // Set value
        let key = "test-key";
        let payload = json!({"value": {"foo": "bar"}, "ttl": 10000});
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/cache/{key}"))
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Get value
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/cache/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["key"], key);
        assert_eq!(val["value"]["foo"], "bar");
    }

    #[tokio::test]
    async fn test_delete_cache_value() {
        let app = app_for_test();
        let key = "delete-key";
        let payload = json!({"value": {"x": 1}, "ttl": 5000});

        // Set value
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/cache/{key}"))
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Delete value
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/cache/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Get value should now return NOT_FOUND
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/cache/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_cache_entries_by_prefix() {
        let app = app_for_test();

        // Set some values with common prefix
        let payload = json!({"value": {"n": 1}, "ttl": 10000});
        for suffix in ["1", "2", "3"] {
            let key = format!("pfx-{suffix}");
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/cache/{key}"))
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
        }

        // List entries by prefix
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/cache?prefix=pfx-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["count"], 3);
    }
}
