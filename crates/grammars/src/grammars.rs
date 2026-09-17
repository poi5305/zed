use std::borrow::Cow;

use anyhow::Context as _;
#[cfg(all(feature = "load-grammars", target_family = "wasm"))]
use language_core::ParseableLanguage;
use language_core::{LanguageConfig, LanguageQueries, QueryFile, QueryFileContents};

// Dev builds read the checkout's query files at runtime instead of embedding
// them; see the `assets` crate for the rationale.
util::fs_embed! {
    struct GrammarDir,
    crate_relative = "src/",
    root_relative = "crates/grammars/src",
    exclude = ["*.rs"],
}

/// Register all built-in native tree-sitter grammars with the provided registration function.
///
/// Each grammar is registered as a `(&str, tree_sitter_language::LanguageFn)` pair.
/// This must be called before loading language configs/queries.
#[cfg(all(feature = "load-grammars", not(target_family = "wasm")))]
pub fn native_grammars() -> Vec<(&'static str, tree_sitter::Language)> {
    vec![
        ("bash", tree_sitter_bash::LANGUAGE.into()),
        ("c", tree_sitter_c::LANGUAGE.into()),
        ("cpp", tree_sitter_cpp::LANGUAGE.into()),
        ("css", tree_sitter_css::LANGUAGE.into()),
        ("diff", tree_sitter_diff::LANGUAGE.into()),
        ("go", tree_sitter_go::LANGUAGE.into()),
        ("gomod", tree_sitter_go_mod::LANGUAGE.into()),
        ("gowork", tree_sitter_gowork::LANGUAGE.into()),
        ("jsdoc", tree_sitter_jsdoc::LANGUAGE.into()),
        ("json", tree_sitter_json::LANGUAGE.into()),
        ("jsonc", tree_sitter_json::LANGUAGE.into()),
        ("markdown", tree_sitter_md::LANGUAGE.into()),
        ("markdown-inline", tree_sitter_md::INLINE_LANGUAGE.into()),
        ("python", tree_sitter_python::LANGUAGE.into()),
        ("regex", tree_sitter_regex::LANGUAGE.into()),
        ("rust", tree_sitter_rust::LANGUAGE.into()),
        ("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
        (
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
        ("yaml", tree_sitter_yaml::LANGUAGE.into()),
        ("gitcommit", tree_sitter_gitcommit::LANGUAGE.into()),
    ]
}

#[cfg(all(feature = "load-grammars", target_family = "wasm"))]
fn parseable(resolve: fn() -> tree_sitter::Language) -> ParseableLanguage {
    ParseableLanguage::from_resolver(std::sync::Arc::new(move || Ok(resolve())))
}

/// Register all built-in native tree-sitter grammars with the provided registration function.
///
/// On wasm each entry is a thread-safe resolver: `tree_sitter::Language` is `!Send` there,
/// so the registry stores `LanguageFn` lookups rather than a language value.
#[cfg(all(feature = "load-grammars", target_family = "wasm"))]
pub fn native_grammars() -> Vec<(&'static str, ParseableLanguage)> {
    vec![
        ("bash", parseable(|| tree_sitter_bash::LANGUAGE.into())),
        ("c", parseable(|| tree_sitter_c::LANGUAGE.into())),
        ("cpp", parseable(|| tree_sitter_cpp::LANGUAGE.into())),
        ("css", parseable(|| tree_sitter_css::LANGUAGE.into())),
        ("diff", parseable(|| tree_sitter_diff::LANGUAGE.into())),
        ("go", parseable(|| tree_sitter_go::LANGUAGE.into())),
        ("gomod", parseable(|| tree_sitter_go_mod::LANGUAGE.into())),
        ("gowork", parseable(|| tree_sitter_gowork::LANGUAGE.into())),
        ("jsdoc", parseable(|| tree_sitter_jsdoc::LANGUAGE.into())),
        ("json", parseable(|| tree_sitter_json::LANGUAGE.into())),
        ("jsonc", parseable(|| tree_sitter_json::LANGUAGE.into())),
        ("markdown", parseable(|| tree_sitter_md::LANGUAGE.into())),
        (
            "markdown-inline",
            parseable(|| tree_sitter_md::INLINE_LANGUAGE.into()),
        ),
        ("python", parseable(|| tree_sitter_python::LANGUAGE.into())),
        ("regex", parseable(|| tree_sitter_regex::LANGUAGE.into())),
        ("rust", parseable(|| tree_sitter_rust::LANGUAGE.into())),
        (
            "tsx",
            parseable(|| tree_sitter_typescript::LANGUAGE_TSX.into()),
        ),
        (
            "typescript",
            parseable(|| tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        ),
        ("yaml", parseable(|| tree_sitter_yaml::LANGUAGE.into())),
        (
            "gitcommit",
            parseable(|| tree_sitter_gitcommit::LANGUAGE.into()),
        ),
    ]
}

/// Load and parse the `config.toml` for a given language name.
pub fn load_config(name: &str) -> LanguageConfig {
    let config_toml = String::from_utf8(
        GrammarDir::get(&format!("{}/config.toml", name))
            .unwrap_or_else(|| panic!("missing config for language {:?}", name))
            .data
            .to_vec(),
    )
    .unwrap();

    let config = LanguageConfig::from_toml(&config_toml)
        .with_context(|| format!("failed to load config.toml for language {name:?}"))
        .unwrap();

    config
}

/// Load and parse the `config.toml` for a given language name, stripping fields
/// that require grammar support when grammars are not loaded.
pub fn load_config_for_feature(name: &str, grammars_loaded: bool) -> LanguageConfig {
    let config = load_config(name);

    if grammars_loaded {
        config
    } else {
        LanguageConfig {
            name: config.name,
            matcher: config.matcher,
            jsx_tag_auto_close: config.jsx_tag_auto_close,
            ..Default::default()
        }
    }
}

/// Get a raw embedded file by path (relative to `src/`).
///
/// Returns the file data as bytes, or `None` if the file does not exist.
pub fn get_file(path: &str) -> Option<rust_embed::EmbeddedFile> {
    GrammarDir::get(path)
}

/// Load all Tree-sitter query files for a given language name.
pub fn load_queries(name: &str) -> LanguageQueries {
    LanguageQueries::from_files(GrammarDir::iter().filter_map(|path| {
        let file_name = path.strip_prefix(name)?.strip_prefix('/')?;
        let query_file = file_name.parse::<QueryFile>().ok()?;
        let contents = match GrammarDir::get(path.as_ref())?.data {
            Cow::Borrowed(bytes) => Cow::Borrowed(std::str::from_utf8(bytes).ok()?),
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8(bytes).ok()?),
        };
        Some(QueryFileContents::new(query_file, contents))
    }))
}
