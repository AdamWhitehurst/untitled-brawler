use std::future::Future;

/// Awaits network futures on the runtime available for the current target.
#[cfg(not(target_arch = "wasm32"))]
pub async fn await_network<F: Future>(future: F) -> F::Output {
    async_compat::Compat::new(future).await
}

/// Awaits browser-backed network futures directly on WASM.
#[cfg(target_arch = "wasm32")]
pub async fn await_network<F: Future>(future: F) -> F::Output {
    future.await
}
