use anyhow::{Error, bail};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
    response::IntoResponse,
    routing::{delete, get, post},
};
use futures::future::{BoxFuture, FutureExt, Shared};
use moka::future::Cache;
use rmp_serde::{decode, encode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tracing_subscriber;

// ------------------ Moka TTL Expiry Section ------------------

pub type CacheValue = (Option<Duration>, Vec<u8>);

pub struct ExpiryPolicyForCacheExt;

impl moka::Expiry<String, CacheValue> for ExpiryPolicyForCacheExt {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &CacheValue,
        _current_time: Instant,
    ) -> Option<Duration> {
        // Return the TTL provided in the cache value.
        value.0
    }
}

pub trait CacheExt<K, V> {
    fn insert_with_ttl<'a>(&'a self, key: K, value: V, ttl: Option<Duration>) -> BoxFuture<'a, ()>;
}

impl CacheExt<String, Vec<u8>> for Cache<String, CacheValue> {
    fn insert_with_ttl<'a>(
        &'a self,
        key: String,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, ()> {
        async move {
            self.insert(key, (ttl, value)).await;
        }
        .boxed()
    }
}

// ------------------ SingleFlight Implementation ------------------

#[derive(Clone)]
struct SingleFlight<T> {
    // Each key maps to a shared future that resolves to a Result<T, Arc<anyhow::Error>>
    inner: Arc<Mutex<HashMap<String, Shared<BoxFuture<'static, Result<T, Arc<Error>>>>>>>,
}

impl<T: Send + 'static> SingleFlight<T> {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn do_call<F>(&self, key: String, f: F) -> Result<T, Arc<Error>>
    where
        // The closure returns a future resulting in T on success.
        F: FnOnce() -> BoxFuture<'static, Result<T, Error>>,
        T: Clone, // Required for Shared to clone the success value.
    {
        let mut guard = self.inner.lock().await;
        if let Some(shared_future) = guard.get(&key) {
            // A call is already in-flight; await its shared result
            return shared_future.clone().await;
        } else {
            // Map errors to Arc<anyhow::Error> for cloning
            let future = f().map(|res| res.map_err(Arc::new)).boxed().shared();
            guard.insert(key.clone(), future.clone());
            drop(guard);
            let result = future.await;
            let mut guard = self.inner.lock().await;
            guard.remove(&key);
            result
        }
    }
}

// ------------------ Application State ------------------

#[derive(Clone)]
struct AppState {
    cache: Cache<String, CacheValue>,
    singleflight: SingleFlight<Vec<u8>>,
}

// ------------------ Request Payloads ------------------

#[derive(Debug, Deserialize)]
struct SetPayload {
    value: serde_json::Value,
    ttl: Option<u64>, // TTL in seconds, optional.
}

// #[derive(Debug, Serialize)]
// struct ApiResponse {
//     key: String,
//     value: String,
//     message: String,
// }

// ------------------ Handlers ------------------

#[derive(Deserialize)]
struct LoadCacheEntriesQueryParams {
    prefix: String,
}

#[derive(Serialize)]
struct LoadCacheEntriesResult {
    key: String,
    ttl: Option<Duration>,
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
                    None => bail!("Key was not found"),
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
                Json(json!({ "key": key_clone, "value": deserialized})),
            ))
        }
        Err(_) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Key not found", "error_code": "KEY_NOT_FOUND" })),
        )),
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
        .insert_with_ttl(key.clone(), serialized, ttl)
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

async fn fallback(uri: Uri) -> impl IntoResponse {
    tracing::error!("No route for {}", uri);
    (
        StatusCode::NOT_FOUND,
        Json(
            json!({ "message": format!("No route for {}", uri), "error_code": "ROUTE_NOT_FOUND" }),
        ),
    )
}

// ------------------ Main ------------------

#[tokio::main]
async fn main() {
    // Initialize tracing subscriber (logging)
    tracing_subscriber::fmt::init();

    let port = std::env::var("PORT").unwrap_or("8080".to_string());

    // Create a `Cache<u32, (Expiration, String)>` with an expiry `MyExpiry` and
    // eviction listener.
    let expiry = ExpiryPolicyForCacheExt;

    let eviction_listener = |key, _value, cause| {
        tracing::debug!("Evicted key {key}. Cause: {cause:?}");
    };

    // Create Moka cache with custom expiration policy.
    let cache: Cache<String, CacheValue> = Cache::builder()
        .weigher(
            |_k: &String, (_ttl, value): &(Option<Duration>, Vec<u8>)| -> u32 {
                value.len().try_into().unwrap_or(u32::MAX)
            },
        )
        .max_capacity(10_000)
        .expire_after(expiry)
        .eviction_listener(eviction_listener)
        // .time_to_live(Duration::from_secs(3600)) // default TTL if not provided; our custom expiry takes precedence.
        .build();

    let app_state = AppState {
        cache,
        singleflight: SingleFlight::new(),
    };

    // Build the Axum router.
    let app = Router::new()
        .route("/cache", get(load_cache_entries))
        .route("/cache/{key}", get(find_cache_by_key))
        .route("/cache/{key}", post(save_cache))
        .route("/cache/{key}", delete(delete_cache))
        .route("/cache/patterns/{text}", delete(invalidate_by_text))
        .fallback(fallback)
        .with_state(app_state);

    // Run server on localhost:8080.
    let addr = SocketAddr::from(([127, 0, 0, 1], port.parse().unwrap()));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    tracing::info!("Starting server on {}", addr);

    axum::serve(listener, app).await.unwrap();
}
