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

# The browser's font database holds only the fonts in zed-assets.tar -- wasm cannot reach the
# system's -- so without a CJK face every Han character is drawn as a missing-glyph box.
# `Assets::load_fonts` loads every .ttf under fonts/, and cosmic-text falls back to any face in
# the database, so shipping the file is the whole fix. It is the static Regular instance: the
# variable font's default instance is Thin, and GPUI never applies a variation.
# The terminal's status glyphs (braille spinner, U+23F5, U+2714, ...) are in neither Lilex nor
# Noto Sans TC, so two more static Regular faces are shipped for them. Both contain 'm', which
# `load_family` requires before it keeps a face loaded by name (the "Noto Sans Symbols 2"
# font_fallbacks entry would otherwise be removed).
cjk_font_root="${ZED_WEB_FONT_CACHE:-${repo_dir}/target/web-fonts}"
cjk_font_dir="${cjk_font_root}/fonts/noto-sans-tc"
cjk_font_url="https://fonts.gstatic.com/s/notosanstc/v39/-nFuOG829Oofr2wohFbTp9ifNAn722rq0MXz76Cy_Co.ttf"
cjk_font_sha256="619662a0583f38311e92666927e5edbfd30f2a1fbe8593685660bd11bdd46a10"
cjk_license_url="https://raw.githubusercontent.com/google/fonts/23e54b51ddffbc7713c583748e3bd86f62b1fa4a/ofl/notosanstc/OFL.txt"
symbols_font_dir="${cjk_font_root}/fonts/noto-sans-symbols-2"
symbols_font_url="https://raw.githubusercontent.com/google/fonts/23e54b51ddffbc7713c583748e3bd86f62b1fa4a/ofl/notosanssymbols2/NotoSansSymbols2-Regular.ttf"
symbols_font_sha256="7d5fb73b7ca67a6798101741f5d280a3d016a56a197afcd4199dbb57b4b82a21"
symbols_license_url="https://raw.githubusercontent.com/google/fonts/23e54b51ddffbc7713c583748e3bd86f62b1fa4a/ofl/notosanssymbols2/OFL.txt"
math_font_dir="${cjk_font_root}/fonts/noto-sans-math"
math_font_url="https://raw.githubusercontent.com/google/fonts/23e54b51ddffbc7713c583748e3bd86f62b1fa4a/ofl/notosansmath/NotoSansMath-Regular.ttf"
math_font_sha256="3f495fe933c06786e4d5f6d86b8ee70b6753a68ee3b9d87528726de0f6e2c47d"
math_license_url="https://raw.githubusercontent.com/google/fonts/23e54b51ddffbc7713c583748e3bd86f62b1fa4a/ofl/notosansmath/OFL.txt"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# ensure_font <label> <dir> <file name> <url> <sha256> <license url>
ensure_font() {
    local label="$1" dir="$2" name="$3" url="$4" expected="$5" license_url="$6"
    local font="${dir}/${name}"
    mkdir -p "${dir}"
    # The whole directory is tarred, so a .partial from an interrupted run must not survive.
    rm -f "${font}.partial" "${dir}/OFL.txt.partial"
    if [[ ! -f "${font}" || "$(sha256_of "${font}")" != "${expected}" ]]; then
        if ! curl -fsSL -o "${font}.partial" "${url}"; then
            rm -f "${font}.partial"
            die "error: downloading ${label} from ${url} failed."
        fi
        local actual
        actual="$(sha256_of "${font}.partial")"
        if [[ "${actual}" != "${expected}" ]]; then
            rm -f "${font}.partial"
            die \
                "error: ${label} checksum mismatch: expected ${expected}, got ${actual}." \
                "If the pin moved on purpose, update the url and sha256 together."
        fi
        mv "${font}.partial" "${font}"
    fi
    if [[ ! -f "${dir}/OFL.txt" ]]; then
        if ! curl -fsSL -o "${dir}/OFL.txt.partial" "${license_url}"; then
            rm -f "${dir}/OFL.txt.partial"
            die "error: downloading the ${label} license from ${license_url} failed."
        fi
        mv "${dir}/OFL.txt.partial" "${dir}/OFL.txt"
    fi
}

ensure_font "Noto Sans TC" "${cjk_font_dir}" NotoSansTC-Regular.ttf \
    "${cjk_font_url}" "${cjk_font_sha256}" "${cjk_license_url}"
ensure_font "Noto Sans Symbols 2" "${symbols_font_dir}" NotoSansSymbols2-Regular.ttf \
    "${symbols_font_url}" "${symbols_font_sha256}" "${symbols_license_url}"
ensure_font "Noto Sans Math" "${math_font_dir}" NotoSansMath-Regular.ttf \
    "${math_font_url}" "${math_font_sha256}" "${math_license_url}"

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

# ---------------------------------------------------------------------------
# remote_server for ssh projects (docs/web-zed-remote-spec.md §4.8 and Q4 in §8.1.1)
# ---------------------------------------------------------------------------
# The server deploys these to the ssh host (dev builds cannot download one), so the remote
# must run this fork's build. Linux binaries are static musl: a glibc build fails on a
# remote whose glibc is older ("version GLIBC_2.xx not found"). The other linux arch is
# musl via cargo-zigbuild when that is installed. A macOS host also builds the other apple
# arch when the rustup target is installed. Windows is best-effort (cargo-zigbuild or
# mingw); otherwise build it on Windows and import it. The layout is read by
# remote_server_bundle.rs from <dir of zed-web-server>/remote/.
# web/scripts/import-remote-server.sh is the only writer of manifest.json.
remote_server_commit_sha="$(git -C "${repo_dir}" rev-parse HEAD 2>/dev/null || true)"
import_remote_server="${web_dir}/scripts/import-remote-server.sh"

host_triple() {
    rustup run "${stable_toolchain}" rustc -vV | sed -n 's/^host: //p'
}

# triple_platform <triple> -> "<os> <arch>", the strings RemoteOs/RemoteArch::as_str use.
triple_platform() {
    case "$1" in
        x86_64-unknown-linux-*) echo "linux x86_64" ;;
        aarch64-unknown-linux-*) echo "linux aarch64" ;;
        x86_64-apple-darwin) echo "macos x86_64" ;;
        aarch64-apple-darwin) echo "macos aarch64" ;;
        x86_64-pc-windows-*) echo "windows x86_64" ;;
        aarch64-pc-windows-*) echo "windows aarch64" ;;
        *) return 1 ;;
    esac
}

# The binary tells its own commit; the manifest must agree with `version` or the server
# would redeploy on every connection.
remote_server_version() {
    "$1" version | awk 'NF { line = $0 } END { print line }' | tr -d '[:space:]'
}

# A glibc-linked remote_server dies on a remote whose glibc is older than this machine's.
# ldd prints "statically linked" for the musl crt-static binary. A cross binary ldd cannot
# load is accepted only when readelf shows no NEEDED library; static-pie musl has a dynamic
# section and still no NEEDED, which is not a glibc dependency.
require_static_linux_binary() {
    local binary="$1" ldd_output readelf_dynamic readelf_bin=""
    if command -v readelf >/dev/null 2>&1; then
        readelf_bin="readelf"
    elif command -v llvm-readelf >/dev/null 2>&1; then
        readelf_bin="llvm-readelf"
    fi

    if command -v ldd >/dev/null 2>&1; then
        ldd_output="$(ldd "${binary}" 2>&1 || true)"
        if printf '%s\n' "${ldd_output}" | grep -q '\.so'; then
            die "error: ${binary} is dynamically linked, so it is not bundled." \
                "${ldd_output}" \
                "A linux remote_server must be a static musl binary. A glibc-linked binary fails on older remotes (GLIBC_x.xx not found)."
        fi
        if printf '%s\n' "${ldd_output}" | grep -q 'statically linked'; then
            return 0
        fi
    fi

    if [[ -z "${readelf_bin}" ]] || ! "${readelf_bin}" -h "${binary}" >/dev/null 2>&1; then
        if ! command -v ldd >/dev/null 2>&1; then
            die "error: neither readelf nor ldd was found, so ${binary} cannot be checked for glibc linkage."
        fi
        die "error: ${binary} is not a static ELF executable, so it is not bundled." \
            "ldd said: ${ldd_output:-<no ldd output>}"
    fi

    local readelf_headers
    readelf_headers="$("${readelf_bin}" -l "${binary}" 2>&1 || true)"
    if printf '%s\n' "${readelf_headers}" | grep -q 'INTERP'; then
        die "error: ${binary} is dynamically linked (has INTERP header), so it is not bundled." \
            "${readelf_headers}" \
            "A linux remote_server must be a static musl binary. A glibc-linked binary fails on older remotes (GLIBC_x.xx not found)."
    fi

    readelf_dynamic="$("${readelf_bin}" -d "${binary}" 2>&1 || true)"
    if printf '%s\n' "${readelf_dynamic}" | grep -q '(NEEDED)'; then
        die "error: ${binary} is dynamically linked (readelf lists NEEDED), so it is not bundled." \
            "${readelf_dynamic}" \
            "A linux remote_server must be a static musl binary. A glibc-linked binary fails on older remotes (GLIBC_x.xx not found)."
    fi
}

# The release profile keeps debuginfo, which makes remote_server ~740 MB; the server uploads
# it over ssh, and without debuginfo it is ~125 MB. Strips a copy so the cargo target keeps
# its symbols. Prints the path to use: the copy, or the original when no strip tool fits.
# strip_debug_info <triple> <native triple> <binary>
strip_debug_info() {
    local triple="$1" native_triple="$2" binary="$3"
    local copy_dir="${native_target}/remote-server-stripped/${triple}"
    local copy
    copy="${copy_dir}/${binary##*/}"
    local -a strip_command
    strip_command=()
    case "${triple}" in
        *-apple-darwin)
            # Apple's strip cross-strips the other apple arch; both slices are bundled.
            if command -v strip >/dev/null 2>&1; then
                strip_command=(strip -S)
            fi
            ;;
        *)
            if command -v "${triple%%-*}-linux-gnu-strip" >/dev/null 2>&1; then
                strip_command=("${triple%%-*}-linux-gnu-strip" --strip-debug)
            elif command -v llvm-strip >/dev/null 2>&1; then
                strip_command=(llvm-strip --strip-debug)
            elif [[ "${triple%%-*}" == "${native_triple%%-*}" ]] && command -v strip >/dev/null 2>&1; then
                # The host-arch musl binary runs here, so the host strip can see it.
                strip_command=(strip --strip-debug)
            fi
            ;;
    esac
    if [[ ${#strip_command[@]} -eq 0 ]]; then
        echo "warning: no strip tool for ${triple}; bundling ${binary} with its debuginfo." >&2
        echo "${binary}"
        return 0
    fi
    mkdir -p "${copy_dir}"
    cp "${binary}" "${copy}"
    if ! "${strip_command[@]}" "${copy}" >&2; then
        echo "warning: ${strip_command[0]} failed on ${triple}; bundling ${binary} with its debuginfo." >&2
        echo "${binary}"
        return 0
    fi
    echo "${copy}"
}

# build_remote_server <triple> [cargo subcommand]: prints the binary's path on success.
build_remote_server() {
    local triple="$1" subcommand="${2:-build}" native_triple="$3"
    local -a target_args
    target_args=()
    if [[ "${triple}" != "${native_triple}" ]]; then
        target_args=(--target "${triple}")
    fi
    (
        cd "${repo_dir}"
        # shellcheck disable=SC2030 # scoped to this subshell on purpose
        if [[ -n "${remote_server_commit_sha}" ]]; then
            export ZED_COMMIT_SHA="${remote_server_commit_sha}"
        fi
        # remote_server prefixes its `version` with these, and the manifest records the bare sha.
        unset GITHUB_RUN_NUMBER ZED_BUILD_ID
        if [[ "${triple}" == *-unknown-linux-musl ]]; then
            # This subshell is the only place RUSTFLAGS is set. The wasm build refuses it,
            # because Cargo replaces web/.cargo/config.toml's rustflags instead of appending.
            # The cc crate reads CC_<triple with '-' as '_'>, lowercase, not CARGO_TARGET_*_LINKER.
            # shellcheck disable=SC2030 # scoped to this subshell on purpose
            export RUSTFLAGS="-C target-feature=+crt-static"
            if [[ "${subcommand}" == build ]]; then
                if ! command -v musl-gcc >/dev/null 2>&1; then
                    printf '%s\n' \
                        "error: musl-gcc was not found. Building a static remote_server requires it; install the musl-tools package." >&2
                    exit 1
                fi
                local musl_cc_var
                musl_cc_var="CC_$(printf '%s' "${triple}" | tr '-' '_')"
                export "${musl_cc_var}=musl-gcc"
            fi
        fi
        # cargo links a foreign windows-gnu target with the host cc unless told otherwise.
        # zigbuild brings its own linker. A linux-gnu gcc is not used: it produces a glibc binary.
        local linker_variable linker_gcc=""
        case "${triple}" in
            x86_64-pc-windows-gnu) linker_gcc="x86_64-w64-mingw32-gcc" ;;
            aarch64-pc-windows-gnu) linker_gcc="aarch64-w64-mingw32-gcc" ;;
        esac
        if [[ -n "${linker_gcc}" && "${triple}" != "${native_triple}" && "${subcommand}" == build ]]; then
            linker_variable="CARGO_TARGET_$(printf '%s' "${triple}" | tr 'a-z-' 'A-Z_')_LINKER"
            if [[ -z "${!linker_variable:-}" ]] && command -v "${linker_gcc}" >/dev/null 2>&1; then
                export "${linker_variable}=${linker_gcc}"
            fi
        fi
        rustup run "${stable_toolchain}" "${cargo_bin}" "${subcommand}" \
            --manifest-path "${repo_dir}/Cargo.toml" \
            --target-dir "${native_target}" \
            --release \
            -p remote_server \
            ${target_args[@]+"${target_args[@]}"} >&2
    ) || return 1
    local output
    if [[ "${triple}" == "${native_triple}" ]]; then
        output="${native_target}/release/remote_server"
    else
        output="${native_target}/${triple}/release/remote_server"
    fi
    if [[ -f "${output}.exe" || "${triple}" == *-pc-windows-* ]]; then
        output="${output}.exe"
    fi
    echo "${output}"
}

# cross_subcommand <triple>: which cargo subcommand can build for a foreign triple here,
# or nothing when no toolchain is installed. Linux gnu is not a result: that binary is
# dynamically linked against this machine's glibc.
cross_subcommand() {
    local triple="$1" linker_gcc="" linker_variable linker_ready
    case "${triple}" in
        *-apple-darwin)
            # Apple's toolchain cross-links the other apple arch; no separate linker.
            local host_os=""
            if platform="$(triple_platform "$(host_triple)")"; then
                read -r host_os _ <<<"${platform}"
            fi
            if [[ "${host_os}" != "macos" ]]; then
                return 1
            fi
            if rustup target list --installed --toolchain "${stable_toolchain}" 2>/dev/null | grep -qx "${triple}"; then
                echo build
                return 0
            fi
            return 1
            ;;
        *-unknown-linux-musl)
            if command -v zig >/dev/null 2>&1 && "${cargo_bin}" zigbuild --help >/dev/null 2>&1; then
                echo zigbuild
                return 0
            fi
            return 1
            ;;
        *-pc-windows-gnu)
            if command -v zig >/dev/null 2>&1 && "${cargo_bin}" zigbuild --help >/dev/null 2>&1; then
                echo zigbuild
                return 0
            fi
            case "${triple}" in
                x86_64-pc-windows-gnu) linker_gcc="x86_64-w64-mingw32-gcc" ;;
                aarch64-pc-windows-gnu) linker_gcc="aarch64-w64-mingw32-gcc" ;;
            esac
            linker_variable="CARGO_TARGET_$(printf '%s' "${triple}" | tr 'a-z-' 'A-Z_')_LINKER"
            linker_ready=0
            if [[ -n "${!linker_variable:-}" ]]; then
                linker_ready=1
            elif [[ -n "${linker_gcc}" ]] && command -v "${linker_gcc}" >/dev/null 2>&1; then
                linker_ready=1
            fi
            if [[ "${linker_ready}" -eq 1 ]] &&
                rustup target list --installed --toolchain "${stable_toolchain}" 2>/dev/null | grep -qx "${triple}"; then
                echo build
                return 0
            fi
            return 1
            ;;
        *)
            return 1
            ;;
    esac
}

build_remote_servers() {
    local native_triple platform os arch binary commit musl_triple native_build_triple
    native_triple="$(host_triple)"
    platform="$(triple_platform "${native_triple}")" ||
        die "error: cannot bundle a remote_server for host triple '${native_triple}'."
    read -r os arch <<<"${platform}"

    native_build_triple="${native_triple}"
    if [[ "${os}" == "linux" ]]; then
        musl_triple="${arch}-unknown-linux-musl"
        if ! command -v musl-gcc >/dev/null 2>&1; then
            die "error: musl-gcc was not found. Building a static remote_server requires it; install the musl-tools package."
        fi
        if ! rustup target list --installed --toolchain "${stable_toolchain}" 2>/dev/null | grep -qx "${musl_triple}"; then
            die "error: rustup target ${musl_triple} is not installed." \
                "A static remote_server is built for that target. Install it with 'rustup target add ${musl_triple}' and rerun; this script does not install targets."
        fi
        native_build_triple="${musl_triple}"
    fi

    echo "Building remote_server for ${native_build_triple}"
    binary="$(build_remote_server "${native_build_triple}" build "${native_triple}")" ||
        die "error: building remote_server for ${native_build_triple} failed."
    binary="$(strip_debug_info "${native_build_triple}" "${native_triple}" "${binary}")"
    commit="$(remote_server_version "${binary}")"
    [[ -n "${commit}" ]] || die "error: '${binary} version' printed nothing."
    if [[ "${os}" == "linux" ]]; then
        require_static_linux_binary "${binary}"
    fi
    ZED_WEB_DIST_DIR="${dist_dir}" "${import_remote_server}" --commit "${commit}" "${binary}" "${os}" "${arch}"

    # The other linux arch is musl via cargo-zigbuild. The other apple arch is a plain
    # cargo --target when that rustup target is installed. A cross binary is imported
    # under the native binary's commit; both are built from the same checkout.
    local extra_triples=""
    if [[ "${os}" == "linux" ]]; then
        if [[ "${arch}" == "x86_64" ]]; then
            extra_triples="aarch64-unknown-linux-musl"
        else
            extra_triples="x86_64-unknown-linux-musl"
        fi
    elif [[ "${os}" == "macos" ]]; then
        if [[ "${arch}" == "x86_64" ]]; then
            extra_triples="aarch64-apple-darwin"
        else
            extra_triples="x86_64-apple-darwin"
        fi
    fi
    extra_triples="${ZED_WEB_REMOTE_SERVER_TARGETS-${extra_triples}}"

    local windows_triple="x86_64-pc-windows-gnu"
    local -a triples
    triples=()
    local triple existing seen
    for triple in ${extra_triples} ${windows_triple}; do
        seen=0
        for existing in "${triples[@]+"${triples[@]}"}"; do
            if [[ "${existing}" == "${triple}" ]]; then
                seen=1
            fi
        done
        if [[ "${seen}" -eq 0 && "${triple}" != "${native_build_triple}" && "${triple}" != "${native_triple}" ]]; then
            triples+=("${triple}")
        fi
    done

    local subcommand cross_binary cross_os cross_arch
    for triple in "${triples[@]+"${triples[@]}"}"; do
        if ! platform="$(triple_platform "${triple}")"; then
            echo "warning: skipping remote_server for ${triple}: not a platform the bundle lists." >&2
            continue
        fi
        read -r cross_os cross_arch <<<"${platform}"
        if ! subcommand="$(cross_subcommand "${triple}")"; then
            case "${triple}" in
                *-unknown-linux-musl)
                    echo "warning: skipping remote_server for ${triple}: cargo-zigbuild and zig are not available." >&2
                    echo "         A glibc cross toolchain is not used: that binary fails on a remote with an older glibc." >&2
                    echo "         The bundle will not list ${cross_os}-${cross_arch} until you build it elsewhere and run web/scripts/import-remote-server.sh." >&2
                    ;;
                *-pc-windows-*)
                    echo "warning: skipping remote_server for ${triple}: no cargo-zigbuild/zig or mingw (${triple%%-*}-w64-mingw32-gcc)." >&2
                    echo "         Build remote_server on Windows and import it with web/scripts/import-remote-server.sh." >&2
                    ;;
                *-apple-darwin)
                    echo "warning: skipping remote_server for ${triple}: the rustup target is not installed." >&2
                    echo "         Install it with 'rustup target add ${triple}' (this script does not install targets), or build it on a Mac and run web/scripts/import-remote-server.sh." >&2
                    ;;
                *)
                    echo "warning: skipping remote_server for ${triple}: no cross toolchain." >&2
                    echo "         The bundle will not list ${cross_os}-${cross_arch} until you build it elsewhere and run web/scripts/import-remote-server.sh." >&2
                    ;;
            esac
            continue
        fi
        echo "Building remote_server for ${triple} (cargo ${subcommand})"
        if ! cross_binary="$(build_remote_server "${triple}" "${subcommand}" "${native_triple}")"; then
            echo "warning: skipping remote_server for ${triple}: the cross build failed (output above)." >&2
            continue
        fi
        cross_binary="$(strip_debug_info "${triple}" "${native_triple}" "${cross_binary}")"
        if [[ "${cross_os}" == "linux" ]]; then
            require_static_linux_binary "${cross_binary}"
        fi
        ZED_WEB_DIST_DIR="${dist_dir}" "${import_remote_server}" --commit "${commit}" "${cross_binary}" "${cross_os}" "${cross_arch}"
    done
}

build_remote_servers

# Wasm app: web workspace. Subshell so the rest of the script is not stuck in web/.
(
    enter_web_cwd_for_wasm
    require_unset_rustflags
    export CARGO_TARGET_DIR="${wasm_target}"
    # The same commit the bundled remote_server reports, so the browser can tell a stale
    # tab from the server it is talking to (§4.8 point 4).
    # shellcheck disable=SC2031 # a different subshell from the remote_server one
    if [[ -n "${remote_server_commit_sha}" ]]; then
        export ZED_COMMIT_SHA="${remote_server_commit_sha}"
    fi
    export CC_wasm32_unknown_unknown="${wasi_sdk}/bin/clang"
    # Headers: tree-sitter's own, not the WASI sysroot's. The sysroot's <wasi/api.h>
    # refuses any target that is not WASI proper, and wasm32-unknown-unknown is not, so
    # every grammar's C failed against it. tree-sitter vendors the libc subset its
    # parsers need for exactly this target -- see its src/wasm-stdlib/README.md, "the
    # same vendored libc sources ... are linked directly into the application" -- and
    # publishes the headers from its `language` crate. WASI clang stays as the compiler.
    #
    # Target features: these must match web/.cargo/config.toml's `-C target-feature`.
    # Rust asks the linker for --shared-memory, and rust-lld refuses it if any object in
    # the link was built without atomics and bulk-memory:
    #   "--shared-memory is disallowed by <obj> because it was not compiled with
    #    'atomics' or 'bulk-memory' features"
    tree_sitter_wasm_headers=$(
        "${cargo_bin}" metadata --format-version 1 --filter-platform wasm32-unknown-unknown 2>/dev/null |
            grep -o '"manifest_path":"[^"]*tree-sitter[^"]*/crates/language/Cargo.toml"' |
            head -1 |
            sed 's/.*:"//; s/"$//; s|/Cargo.toml$|/wasm/include|'
    )
    if [[ -z "${tree_sitter_wasm_headers}" || ! -d "${tree_sitter_wasm_headers}" ]]; then
        die "error: could not locate tree-sitter's wasm headers via cargo metadata." \
            "They come from the \`tree-sitter-language\` crate's wasm/include directory," \
            "and every tree-sitter grammar's C build needs them on wasm32-unknown-unknown."
    fi
    # -include grammar-wasm-compat.h: tree-sitter-language's wasm <ctype.h> has
    # no isdigit; bash 0.25.1 and the markdown scanner need it. See that header.
    export CFLAGS_wasm32_unknown_unknown="-isystem ${tree_sitter_wasm_headers} -include ${web_dir}/grammar-wasm-compat.h -matomics -mbulk-memory -mmutable-globals"
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
    fonts icons images themes sounds prompts \
    -C "${cjk_font_root}" fonts

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
