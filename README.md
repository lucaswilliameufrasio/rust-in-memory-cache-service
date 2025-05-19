# 🦀 Rust in memory cache service

A high-performance, in-memory key-value cache service built with [Axum](https://github.com/tokio-rs/axum) and [Moka](https://github.com/moka-rs/moka), supporting per-key TTL, REST API access, and disk snapshots for persistent state between server restarts.

---

## ✨ Features

* **Lightning-fast** in-memory cache with per-key TTL (time-to-live)
* **Snapshot & Restore:** Persists cache state to disk on shutdown and restores it on startup, including TTLs
* **RESTful API:** Set, get, list, and invalidate cache entries with HTTP requests
* **Concurrent-safe** with request de-duplication (singleflight)
* **Eviction policy** with custom expiry logic
* **Thoroughly tested** snapshot load/save logic (see `tests`)

---

## 🚀 Quick Start

### 1. Clone and Build

```bash
git clone https://github.com/lucaswilliameufrasio/rust-in-memory-cache-service.git
cd rust-in-memory-cache-service
cargo build --release
```

### 2. Run the Server

```bash
cargo run --release
```

The server listens on port `8080` by default. Change this with the `PORT` environment variable.

---

## 🧑‍💻 Development

To ensure code quality and consistency, use the following commands before committing:

```bash
cargo fmt --all        # Format code according to Rust style
cargo clippy -- -D warnings   # Lint code and fail on any warnings

---

## 🛣️ API Endpoints

### Set Cache Value

```http
POST /cache/{key}
Content-Type: application/json

{
  "value": <any valid JSON>,
  "ttl": 60000      // TTL in milliseconds (optional)
}
```

### Get Cache Value

```http
GET /cache/{key}
```

### List Cache Entries by Prefix

```http
GET /cache?prefix={prefix}
```

### Delete Cache Entry

```http
DELETE /cache/{key}
```

### Invalidate By Pattern

```http
DELETE /cache/patterns/{text}
```

### Health Check

```http
GET /health-check
```

---

## 💾 Persistence (Snapshot) Mechanism

* **On Shutdown:** The server saves all cache entries (including each entry’s expiration time) to a snapshot file (`cache-snapshot.rmp` by default).
* **On Startup:** The server loads the snapshot file and restores only unexpired entries with the correct remaining TTL.
* **TTL Integrity:** Entries expire at the same time as if the server had never restarted.
* **Configurable Location:** Set `SNAPSHOT_LOCATION` environment variable to change the file path.

---

## ⚡ How It Works (Technical Overview)

* **Moka Cache:** Efficient concurrent cache with custom expiry policy, supporting per-key TTL.
* **Axum:** Handles HTTP routing and request parsing.
* **rmp-serde:** For compact binary snapshot serialization.
* **chrono:** Used to record absolute expiration times per entry (not just TTL), so restored entries have correct lifetimes.

**Snapshot Example (Simplified):**

```rust
#[derive(Serialize, Deserialize)]
struct SnapshotEntry {
    key: String,
    value: Vec<u8>,
    expires_at: Option<DateTime<Utc>>,
}
```

---

## 🧪 Testing

To run all tests (including snapshot persistence tests):

```bash
cargo test
```

Example test for snapshot save/load logic:

```rust
#[tokio::test]
async fn test_snapshot_save_and_load() {
    use chrono::{Utc, Duration as ChronoDuration};
    use moka::future::Cache;
    use tempfile::NamedTempFile;
    
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
    save_cache_snapshot(snapshot_path.clone(), &cache).await.unwrap();

    // Create a new cache and load from the snapshot
    let loaded_cache: Cache<String, CacheValue> = Cache::builder().max_capacity(10).build();
    load_cache_snapshot(snapshot_path, &loaded_cache).await.unwrap();

    // The value should be present in the loaded cache
    let loaded = loaded_cache.get(&key).await;
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().1, value);
}
```

### API Integration Tests

You can run tests that exercise the HTTP API using `cargo test`:

```rust
#[tokio::test]
async fn test_set_and_get_cache_value() {
    // Build the app
    let app = app_for_test();

    // Set value
    let key = "test-key";
    let payload = json!({"value": {"foo": "bar"}, "ttl": 10000});
    let res = app
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
    assert_eq!(res.status(), StatusCode::OK);

    // Get value
    let res = app
        .oneshot(
            Request::builder()
                .uri(format!("/cache/{key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = hyper::body::to_bytes(res.into_body()).await.unwrap();
    let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(val["key"], key);
    assert_eq!(val["value"]["foo"], "bar");
}


---

## 🛠️ Customization

* **Max Cache Capacity:** Change `.max_capacity(10_000)` in `main.rs` as needed.
* **Snapshot Path:** Set the `SNAPSHOT_LOCATION` environment variable, or edit the fallback string in your code.
* **Snapshot Interval:** Add a periodic background task if you want auto-snapshots (not just on shutdown).

---

## ⚠️ Caveats

* **Snapshot file is overwritten on shutdown.** To avoid data loss, consider backup or rotation.
* **Snapshot interval:** By default, snapshots only happen on graceful shutdown (`Ctrl+C`). Abrupt kills may lose recent cache data.
* **All cache values are serialized as MessagePack binary (not plain JSON).**

---

## 📄 License

MIT

---

## 🙏 Credits

* [Axum](https://github.com/tokio-rs/axum)
* [Moka](https://github.com/moka-rs/moka)
* [Serde](https://github.com/serde-rs/serde)
* [chrono](https://github.com/chronotope/chrono)
* [rmp-serde](https://github.com/3Hren/msgpack-rust)

---

**Happy caching!**
