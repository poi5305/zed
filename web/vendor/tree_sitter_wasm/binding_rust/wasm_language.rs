#[cfg(not(target_family = "wasm"))]
include!("wasm_language_native.rs");

#[cfg(target_family = "wasm")]
mod stub {
    use std::{error, fmt};

    pub mod wasmtime {
        #[derive(Debug)]
        pub struct Config;

        impl Config {
            pub fn new() -> Self {
                Self
            }
        }

        #[derive(Debug)]
        pub struct Engine;

        impl Engine {
            pub fn new(_: &Config) -> Result<Self, String> {
                Ok(Self)
            }
        }
    }

    #[derive(Debug)]
    pub struct WasmStore;

    impl WasmStore {
        pub fn new(_engine: &wasmtime::Engine) -> Result<Self, WasmError> {
            Ok(Self)
        }

        pub fn load_language(
            &mut self,
            _name: &str,
            _bytes: &[u8],
        ) -> Result<crate::Language, WasmError> {
            Err(WasmError {
                kind: WasmErrorKind::Other,
                message: "WASM grammars are not supported in the browser".to_string(),
            })
        }

        #[must_use]
        pub fn language_count(&self) -> usize {
            0
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    pub struct WasmError {
        pub kind: WasmErrorKind,
        pub message: String,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum WasmErrorKind {
        Parse,
        Compile,
        Instantiate,
        Other,
    }

    impl fmt::Display for WasmError {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let kind = match self.kind {
                WasmErrorKind::Parse => "Failed to parse Wasm",
                WasmErrorKind::Compile => "Failed to compile Wasm",
                WasmErrorKind::Instantiate => "Failed to instantiate Wasm module",
                WasmErrorKind::Other => "Unknown error",
            };
            write!(f, "{kind}: {}", self.message)
        }
    }

    impl error::Error for WasmError {}

    impl crate::Language {
        #[must_use]
        pub fn is_wasm(&self) -> bool {
            false
        }
    }

    impl crate::Parser {
        pub fn set_wasm_store(&mut self, _store: WasmStore) -> Result<(), crate::LanguageError> {
            Ok(())
        }

        pub fn take_wasm_store(&mut self) -> Option<WasmStore> {
            Some(WasmStore)
        }
    }
}

#[cfg(target_family = "wasm")]
pub use stub::*;
