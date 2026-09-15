//! Stand-in for Phase 4's `wasm_rpc::RpcClient`.
//!
//! `fs` / `process` were taken from zed-web's wrapper and call this surface.
//! The real client cannot be a path dependency until `wasm_rpc` exists. These
//! methods keep the same signatures so Phase 4 can replace this module with
//! `pub use wasm_rpc::RpcClient`.

use anyhow::{Result, anyhow};
use futures::channel::mpsc;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct RpcClient;

impl RpcClient {
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        _params: &P,
    ) -> Result<R> {
        Err(anyhow!(
            "smol wasm RPC is not wired yet (no wasm_rpc crate); call {method} dropped"
        ))
    }

    pub async fn call_void<P: Serialize>(&self, method: &str, params: &P) -> Result<()> {
        self.call::<_, Value>(method, params).await?;
        Ok(())
    }

    pub fn on_notification<F: Fn(Value) + Send + 'static>(&self, _method: &str, _handler: F) {}

    pub fn subscribe_reconnect(&self) -> mpsc::UnboundedReceiver<u64> {
        let (_sender, receiver) = mpsc::unbounded();
        receiver
    }
}
