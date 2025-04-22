use anyhow::{bail, Error};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, Uri},
    response::IntoResponse,
    routing::{delete, get, post},
};
use futures::future::{BoxFuture, FutureExt, Shared};
use moka::future::Cache;
use serde::Deserialize;
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
use rmp_serde::{decode, encode};

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
            let future = f()
                .map(|res| res.map_err(Arc::new))
                .boxed()
                .shared();
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

async fn get_cache_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // If not present in cache, use singleflight to deduplicate concurrent fetches.
    // let ttl_duration = Duration::from_secs(5);
    let key_clone = key.clone();
    // let fetch_future = || async move {
    //     // Simulate a slow backend call.
    //     tokio::time::sleep(Duration::from_secs(2)).await;
    //     Ok(format!(
    //         "Fetched value for key {} at {}",
    //         key_clone,
    //         chrono::Utc::now().to_rfc2822()
    //     ))
    // }
    // .boxed();

    // match state.singleflight.do_call(key.clone(), fetch_future).await {
    //     Ok(fetched_value) => {
    //         // Insert fetched value into cache using our TTL extension.
    //         state
    //             .cache
    //             .insert_with_ttl(key.clone(), fetched_value.clone(), Some(ttl_duration))
    //             .await;
    //         Ok((StatusCode::OK, format!("Key: {}\nValue: {}\n", key, fetched_value)))
    //     }
    //     Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    // }

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
            // tracing::debug!("Oxi cached_value {:?}", cached_value);

            let deserialized = decode::from_slice::<serde_json::Value>(&cached_value).unwrap();

            // tracing::debug!("Oxi deserialized {:?}", deserialized);

            
            Ok((
            StatusCode::OK,
            Json(json!({ "key": key_clone, "value": deserialized})),
        ))},
        Err(_) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Key not found", "error_code": "KEY_NOT_FOUND" })),
        )),
    }
}

async fn post_cache_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(payload): Json<SetPayload>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let ttl = payload.ttl.map(Duration::from_secs);
    let serialized = encode::to_vec(&payload.value).unwrap();
    // tracing::debug!("Oxi {:?} {}", serialized, payload.value);

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

async fn delete_cache_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse, Infallible> {
    state.cache.invalidate(&key).await;
    Ok((
        StatusCode::OK,
        Json(json!({ "message": format!("Cache key {} deleted.", key) })),
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

    // Create Moka cache with custom expiration policy.
    let cache: Cache<String, CacheValue> = Cache::builder()
        .weigher(|_k, (_ttl, _v)| 1)
        .max_capacity(100)
        .time_to_live(Duration::from_secs(3600)) // default TTL if not provided; our custom expiry takes precedence.
        .build();

    let app_state = AppState {
        cache,
        singleflight: SingleFlight::new(),
    };

    // Build the Axum router.
    let app = Router::new()
        .route("/cache/{key}", get(get_cache_handler))
        .route("/cache/{key}", post(post_cache_handler))
        .route("/cache/{key}", delete(delete_cache_handler))
        .fallback(fallback)
        .with_state(app_state);

    // Run server on localhost:8080.
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    tracing::info!("Starting server on {}", addr);

    axum::serve(listener, app).await.unwrap();
}
