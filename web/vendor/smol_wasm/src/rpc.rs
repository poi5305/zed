//! The RPC client `fs` and `process` call through.
//!
//! Phase 2 wrote a stand-in here with these exact signatures, to be replaced by
//! `wasm_rpc::RpcClient` once that crate existed. This is that replacement: the
//! stand-in's `call` returned `Err("smol wasm RPC is not wired yet")`, so every
//! remote filesystem and process operation in the browser failed until now.

pub use wasm_rpc::RpcClient;
