#!/usr/bin/env bash
# Web Zed build entrypoint for the *second* Cargo workspace (docs/web-zed-plan.md §3.2).
#
# zedweb/zed-web:web/build.sh cannot be copied: it builds -p zed_web_workspace from the
# *root* manifest with export RUSTFLAGS=…. Our wasm flags live in web/.cargo/config.toml
# under [target.wasm32-unknown-unknown], which Cargo discovers by walking up from the
# current working directory, not from --manifest-path (F3). Env RUSTFLAGS replaces that
# list instead of concatenating (F6 / Cargo precedence), so this script never exports it.
set -euo pipefail

die() {
    printf '%s\n' "$@" >&2
    exit 1
}

web_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd "${web_dir}/.." && pwd)"
dist_dir="${ZED_WEB_DIST_DIR:-${web_dir}/dist}"
static_dir="${dist_dir}/static"
native_target="${ZED_WEB_NATIVE_TARGET:-${repo_dir}/target/web-native}"
# Dedicated dir, never the desktop target/ and never web/target (§2.4 / §9).
# Do not default to CARGO_TARGET_DIR: a caller pointing that at target/ would
# share artifacts with the desktop build.
wasm_target="${ZED_WEB_WASM_TARGET:-${repo_dir}/target/web-wasm}"
wasi_sdk="${WASI_SDK_PATH:-${repo_dir}/target/wasi-sdk}"
profile="${ZED_WEB_PROFILE:-web-release}"
stable_toolchain="${RUST_STABLE_TOOLCHAIN:-1.97.1}"
# Floating "nightly", overridable to a date pin (nightly-YYYY-MM-DD) once a build
# has succeeded. Not a web/rust-toolchain.toml — that would pin rust-analyzer and
# every `cd web && cargo` to nightly (F4). rustup run beats the repo pin via
# RUSTUP_TOOLCHAIN without --install.
nightly_toolchain="${RUST_NIGHTLY_TOOLCHAIN:-nightly}"
# The CLI and the `wasm-bindgen` crate must be the same version -- wasm-bindgen refuses
# to process a module built against a different bindgen schema -- so read the requirement
# from the lock rather than repeating it here. A hard-coded number is one more place for
# the two to disagree, and they did: this said 0.2.127 while both locks pinned 0.2.120.
wasm_bindgen_version="${WASM_BINDGEN_VERSION:-}"
if [[ -z "${wasm_bindgen_version}" ]]; then
    wasm_bindgen_version=$(
        awk '/^name = "wasm-bindgen"$/ { found = 1; next }
             found && /^version = / { gsub(/[">]|version = /, ""); print; exit }' \
            "${repo_dir}/web/Cargo.lock"
    )
fi
if [[ -z "${wasm_bindgen_version}" ]]; then
    die "error: could not read the wasm-bindgen version from web/Cargo.lock."
fi
cargo_bin="${CARGO:-cargo}"
download_wasi_sdk="${repo_dir}/script/download-wasi-sdk"

# ---------------------------------------------------------------------------
# Environment that would silently strip the wasm rustflags
# ---------------------------------------------------------------------------
# Cargo rustflags sources are mutually exclusive, first match wins:
#   CARGO_ENCODED_RUSTFLAGS > RUSTFLAGS > concatenated target.* > build.rustflags
# Our 13 flags (+atomics, shared memory, getrandom wasm_js, …) live in
# web/.cargo/config.toml [target.wasm32-unknown-unknown]. Exporting RUSTFLAGS —
# even the same string the reference script uses — *replaces* that list.
require_unset_rustflags() {
    if [[ -n "${RUSTFLAGS+x}" ]]; then
        die \
            "error: RUSTFLAGS is set in the environment (${RUSTFLAGS:-<empty>})." \
            "Cargo replaces [target.*] rustflags with RUSTFLAGS rather than concatenating," \
            "so web/.cargo/config.toml's +atomics / shared-memory list would be dropped." \
            "Unset it and rerun:  unset RUSTFLAGS"
    fi
    if [[ -n "${CARGO_ENCODED_RUSTFLAGS+x}" ]]; then
        die \
            "error: CARGO_ENCODED_RUSTFLAGS is set in the environment." \
            "It outranks RUSTFLAGS and [target.*] rustflags, so the web wasm flags would" \
            "not apply. Unset it and rerun:  unset CARGO_ENCODED_RUSTFLAGS"
    fi
}

# ---------------------------------------------------------------------------
# F4 — nightly + rust-src for -Z build-std=std,panic_abort
# ---------------------------------------------------------------------------
# Selected with `rustup run`, not `cargo +nightly` and not web/rust-toolchain.toml:
# - rustup run matches zedweb/zed-web:web/build.sh:53 and sets RUSTUP_TOOLCHAIN
#   (rustup override slot 2), which beats the repo rust-toolchain.toml (slot 4).
# - cargo +nightly is equivalent (slot 1) but is not what the reference uses.
# - web/rust-toolchain.toml channel=nightly would pin rust-analyzer and every
#   cargo invocation under web/ to nightly. F4's review already refused that.
# Never pass rustup run --install; missing nightly must fail here, not download.
require_nightly() {
    local rustc_version
    if ! rustc_version="$(rustup run "${nightly_toolchain}" rustc --version 2>&1)"; then
        die \
            "error: nightly Rust is not installed (toolchain '${nightly_toolchain}')." \
            "${rustc_version}" \
            "" \
            "+atomics requires rebuilding std with -Z build-std=std,panic_abort, which" \
            "only nightly cargo can do. The repo rust-toolchain.toml stays on stable" \
            "1.97.1; this script selects nightly for the wasm cargo line only." \
            "" \
            "Install (this script will not do it):" \
            "  rustup toolchain install ${nightly_toolchain}" \
            "  rustup component add rust-src --toolchain ${nightly_toolchain}" \
            "  rustup target add wasm32-unknown-unknown --toolchain ${nightly_toolchain}"
    fi
    if ! rustup component list --installed --toolchain "${nightly_toolchain}" 2>/dev/null | grep -qx 'rust-src'; then
        die \
            "error: toolchain '${nightly_toolchain}' is installed (${rustc_version}) but rust-src is not." \
            "-Z build-std=std,panic_abort needs the std sources." \
            "Install:  rustup component add rust-src --toolchain ${nightly_toolchain}"
    fi
}

# ---------------------------------------------------------------------------
# §4.2 — WASI SDK is a hard prerequisite (18 tree-sitter grammar C libraries)
# ---------------------------------------------------------------------------
# Host clang has no wasm32-wasi headers. The wasm graph enables load-grammars via
# crates/markdown and crates/edit_prediction, which zed_web_workspace reaches
# through editor / markdown_preview / agent_ui.
# script/download-wasi-sdk exists in this tree (WASI SDK v25) and writes
# ./target/wasi-sdk relative to *its* cwd, so it must be run from the repo root.
# This script does not invoke it: a download is a heavyweight network operation.
require_wasi_sdk() {
    if [[ ! -x "${wasi_sdk}/bin/clang" ]]; then
        die \
            "error: WASI SDK clang not found at ${wasi_sdk}/bin/clang." \
            "The web wasm build compiles 18 tree-sitter grammar C libraries for" \
            "wasm32-unknown-unknown and needs WASI clang plus wasi-sysroot headers." \
            "This script will not download the SDK." \
            "" \
            "From the repo root (the downloader writes ./target/wasi-sdk relative to cwd):" \
            "  ${download_wasi_sdk}" \
            "Or set WASI_SDK_PATH to an existing WASI SDK v25 install."
    fi
    if [[ ! -d "${wasi_sdk}/share/wasi-sysroot/include/wasm32-wasi" ]]; then
        die \
            "error: WASI sysroot headers missing at ${wasi_sdk}/share/wasi-sysroot/include/wasm32-wasi." \
            "CFLAGS_wasm32_unknown_unknown needs that include path." \
            "Reinstall from the repo root:  ${download_wasi_sdk}"
    fi
}

require_wasm_bindgen_cli() {
    local installed
    installed="$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)"
    if [[ "${installed}" != "${wasm_bindgen_version}" ]]; then
        die \
            "error: wasm-bindgen-cli ${wasm_bindgen_version} is required (found ${installed:-not installed})." \
            "Install:  cargo install wasm-bindgen-cli --version ${wasm_bindgen_version} --locked"
    fi
}

# F3: after this function returns, cwd is web_dir and the web config is the one
# cargo will find. Do not replace the cd with --manifest-path from the repo root.
enter_web_cwd_for_wasm() {
    cd "${web_dir}"
    if [[ "${PWD}" != "${web_dir}" ]]; then
        die \
            "error: wasm cargo cwd is ${PWD}, expected ${web_dir}." \
            "Cargo discovers .cargo/config.toml by walking up from cwd, not from --manifest-path." \
            "A mismatch means the 13 wasm rustflags would not apply (F3)."
    fi
    if [[ "${PWD}" == "${repo_dir}" ]]; then
        die \
            "error: refusing to run wasm cargo from the repo root (F3)." \
            "That invocation silently applies the desktop rustflags" \
            "(-C symbol-mangling-version=v0 --cfg tokio_unstable) instead of +atomics."
    fi
    if [[ ! -f "${PWD}/.cargo/config.toml" ]]; then
        die \
            "error: ${PWD}/.cargo/config.toml is missing." \
            "Without it cargo walks up to the desktop .cargo/config.toml (F3)."
    fi
    if ! grep -q 'target-feature=+atomics' "${PWD}/.cargo/config.toml"; then
        die \
            "error: ${PWD}/.cargo/config.toml does not list +atomics." \
            "Refusing to produce a wasm binary without atomics / shared memory."
    fi
}

revision() {
    local fallback="$1"
    shift
    git -C "${repo_dir}" "$@" 2>/dev/null || printf '%s' "${fallback}"
}

# ---------------------------------------------------------------------------
# Identity + caller environment (no cargo, no downloads)
# ---------------------------------------------------------------------------
[[ -f "${web_dir}/Cargo.toml" ]] || die "error: ${web_dir}/Cargo.toml is missing."
[[ -f "${web_dir}/.cargo/config.toml" ]] || die "error: ${web_dir}/.cargo/config.toml is missing."
command -v rustup >/dev/null 2>&1 || die "error: rustup is required but not on PATH."

require_unset_rustflags
require_nightly
require_wasi_sdk
require_wasm_bindgen_cli

if [[ ! -f "${web_dir}/static/workspace.html" ]]; then
    die \
        "error: ${web_dir}/static/workspace.html is missing." \
        "Copy it from the zed-web reference as part of the Phase 4 static assets."
fi
if [[ ! -x "${web_dir}/scripts/patch-wasm-bindgen-memory.sh" ]]; then
    die \
        "error: ${web_dir}/scripts/patch-wasm-bindgen-memory.sh is missing or not executable." \
        "Copy it from the zed-web reference; the bindgen JS must import a 128 MiB shared memory."
fi

web_revision="$(revision "${WEB_REVISION:-unknown}" rev-parse HEAD)"
if [[ -f "${web_dir}/upstream-revision" ]]; then
    recorded_upstream_revision="$(tr -d '[:space:]' < "${web_dir}/upstream-revision")"
else
    recorded_upstream_revision="unknown"
fi
upstream_revision="$(revision "${UPSTREAM_REVISION:-${recorded_upstream_revision}}" merge-base HEAD upstream/main)"

rm -rf "${dist_dir}"
mkdir -p "${dist_dir}/bin" "${static_dir}"
install -m 0644 "${web_dir}/static/workspace.html" "${static_dir}/workspace.html"

# Native server: root workspace, dedicated target dir. Not a web/ member (§3.2).
# extension_runtime_cli is not in this tree; zed_web_server starts without it.
(
    cd "${repo_dir}"
    rustup run "${stable_toolchain}" "${cargo_bin}" build \
        --manifest-path "${repo_dir}/Cargo.toml" \
        --target-dir "${native_target}" \
        --release \
        -p zed_web_server
)
install -m 0755 \
    "${native_target}/release/zed-web-server" \
    "${dist_dir}/bin/zed-web-server"

# Wasm app: web workspace. Subshell so the rest of the script is not stuck in web/.
(
    enter_web_cwd_for_wasm
    require_unset_rustflags
    export CARGO_TARGET_DIR="${wasm_target}"
    export CC_wasm32_unknown_unknown="${wasi_sdk}/bin/clang"
    # The C target features must match web/.cargo/config.toml's `-C target-feature`.
    # Rust asks the linker for --shared-memory, and rust-lld refuses it if any object in
    # the link was built without atomics and bulk-memory:
    #   "--shared-memory is disallowed by <obj> because it was not compiled with
    #    'atomics' or 'bulk-memory' features"
    export CFLAGS_wasm32_unknown_unknown="-isystem ${wasi_sdk}/share/wasi-sysroot/include/wasm32-wasi -matomics -mbulk-memory -mmutable-globals"
    rustup run "${nightly_toolchain}" "${cargo_bin}" build \
        -p zed_web_workspace \
        --target wasm32-unknown-unknown \
        --profile "${profile}" \
        -Z build-std=std,panic_abort
)

wasm-bindgen \
    --target web \
    --no-typescript \
    --out-dir "${static_dir}" \
    "${wasm_target}/wasm32-unknown-unknown/${profile}/zed_web_workspace.wasm"
"${web_dir}/scripts/patch-wasm-bindgen-memory.sh" \
    "${static_dir}/zed_web_workspace.js"

COPYFILE_DISABLE=1 tar -C "${repo_dir}/assets" \
    --exclude='._*' \
    --exclude='.DS_Store' \
    -cf "${static_dir}/zed-assets.tar" \
    fonts icons images themes sounds prompts

printf '{\n  "web_revision": "%s",\n  "upstream_revision": "%s"\n}\n' \
    "${web_revision}" \
    "${upstream_revision}" \
    > "${static_dir}/build-info.json"

wasm="${static_dir}/zed_web_workspace_bg.wasm"
raw="$(wc -c < "${wasm}" | tr -d ' ')"
gzip_size="$(gzip -9 -c "${wasm}" | wc -c | tr -d ' ')"
for asset in \
    "${static_dir}/workspace.html" \
    "${static_dir}/zed_web_workspace.js" \
    "${static_dir}/zed-assets.tar" \
    "${wasm}"; do
    gzip -9 -c "${asset}" > "${asset}.gz"
    if command -v brotli >/dev/null 2>&1; then
        brotli -q "${BROTLI_QUALITY:-11}" -f -o "${asset}.br" "${asset}"
    fi
done

brotli_size="unavailable"
if [[ -f "${wasm}.br" ]]; then
    brotli_size="$(wc -c < "${wasm}.br" | tr -d ' ')"
fi
printf 'Zed Web WASM raw:    %s bytes\n' "${raw}"
printf 'Zed Web WASM gzip:   %s bytes\n' "${gzip_size}"
printf 'Zed Web WASM brotli: %s bytes\n' "${brotli_size}"
printf 'Distribution:        %s\n' "${dist_dir}"
