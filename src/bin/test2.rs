use futures::future::{BoxFuture, FutureExt, Shared};
use std::{
    collections::HashMap,
    sync::Arc,
};
use tokio::sync::Mutex;
use anyhow::Error;

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

// ---------- Example usage ----------
use tokio::time::{sleep, Duration};
use anyhow::anyhow;

#[tokio::main]
async fn main() {
    // Here, T will be i32 (for example). You can change it to any type.
    let singleflight: SingleFlight<i32> = SingleFlight::new();

    // Example closure that simulates a slow (2-second) computation that returns 42.
    let key = "compute42".to_string();
    let closure = || async {
        sleep(Duration::from_secs(2)).await;
        // You can return an error with: Err(anyhow!("some error"))
        Ok(42)
    }
    .boxed();

    // Execute the singleflight call.
    let result = singleflight.do_call(key, closure).await;
    match result {
        Ok(value) => println!("Obtained value: {}", value),
        Err(e) => println!("Encountered an error: {}", e),
    }
}