// This crate was essentially pulled out verbatim from main `zed` crate to avoid having to run RustEmbed macro whenever zed has to be rebuilt. It saves a second or two on an incremental build.

use anyhow::Context as _;
use gpui::{App, AssetSource, Result, SharedString};

#[cfg(target_family = "wasm")]
use std::collections::BTreeMap;
#[cfg(target_family = "wasm")]
use std::sync::OnceLock;

// Release builds embed the assets; dev builds read them from the checkout at
// runtime so edits show up on the next launch without a rebuild and no
// build-time path is baked in (which corgi's sandbox rejects). See
// `util::fs_embed!`.
util::fs_embed! {
    pub struct Assets,
    crate_relative = "../../assets",
    root_relative = "assets",
    include = [
        "fonts/**/*",
        "icons/**/*",
        "images/**/*",
        "themes/**/*",
        "sounds/**/*",
        "prompts/**/*",
        "*.md",
    ],
    exclude = ["themes/src/*", "*.DS_Store"],
}

#[cfg(target_family = "wasm")]
static WEB_ASSETS: OnceLock<BTreeMap<String, Vec<u8>>> = OnceLock::new();

/// Install a runtime overlay consulted before the `fs_embed!` store.
///
/// Wasm cannot read the checkout, and embedding the full asset tree in the
/// binary is too large, so the web entry point fetches a tar and calls this.
#[cfg(target_family = "wasm")]
pub fn install_web_assets(assets: BTreeMap<String, Vec<u8>>) -> anyhow::Result<()> {
    WEB_ASSETS
        .set(assets)
        .map_err(|_| anyhow::anyhow!("web assets already installed"))
}

#[cfg(target_family = "wasm")]
fn web_asset(path: &str) -> Option<Vec<u8>> {
    WEB_ASSETS.get()?.get(path).cloned()
}

#[cfg(target_family = "wasm")]
fn merge_web_overlay_paths(mut paths: Vec<SharedString>, prefix: &str) -> Vec<SharedString> {
    let Some(overlay) = WEB_ASSETS.get() else {
        return paths;
    };
    for overlay_path in overlay.keys() {
        if overlay_path.starts_with(prefix)
            && !paths
                .iter()
                .any(|existing| existing.as_ref() == overlay_path)
        {
            paths.push(overlay_path.clone().into());
        }
    }
    paths
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        #[cfg(target_family = "wasm")]
        if let Some(bytes) = web_asset(path) {
            return Ok(Some(std::borrow::Cow::Owned(bytes)));
        }
        Self::get(path)
            .map(|f| Some(f.data))
            .with_context(|| format!("loading asset at path {path:?}"))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let paths = Self::iter()
            .filter_map(|p| {
                if p.starts_with(path) {
                    Some(p.into())
                } else {
                    None
                }
            })
            .collect();
        #[cfg(target_family = "wasm")]
        let paths = merge_web_overlay_paths(paths, path);
        Ok(paths)
    }
}

impl Assets {
    /// Populate the [`TextSystem`] of the given [`AppContext`] with all `.ttf` fonts in the `fonts` directory.
    pub fn load_fonts(&self, cx: &App) -> anyhow::Result<()> {
        let font_paths = self.list("fonts")?;
        let mut embedded_fonts = Vec::new();
        for font_path in font_paths {
            if font_path.ends_with(".ttf") {
                let font_bytes = cx
                    .asset_source()
                    .load(&font_path)?
                    .expect("Assets should never return None");
                embedded_fonts.push(font_bytes);
            }
        }

        cx.text_system().add_fonts(embedded_fonts)
    }

    pub fn load_test_fonts(&self, cx: &App) {
        cx.text_system()
            .add_fonts(vec![
                self.load("fonts/lilex/Lilex-Regular.ttf").unwrap().unwrap(),
            ])
            .unwrap()
    }
}
