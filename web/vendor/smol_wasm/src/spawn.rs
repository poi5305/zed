//! WASM stub for `smol::spawn`. Native targets use `pub use smol_real::*`.

use std::future::Future;

/// WASM stub: returns a task that never resolves.
pub fn spawn<T: 'static>(future: impl Future<Output = T> + 'static) -> WasmTask<T> {
    let _ = Box::new(future);
    WasmTask {
        _phantom: std::marker::PhantomData,
    }
}

#[derive(Debug)]
pub struct WasmTask<T> {
    _phantom: std::marker::PhantomData<T>,
}

impl<T> WasmTask<T> {
    pub fn detach(self) {}

    pub async fn cancel(self) -> Option<T> {
        None
    }
}

impl<T> Future for WasmTask<T> {
    type Output = T;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Pending
    }
}
