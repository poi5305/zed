#!/usr/bin/env bash
# Runs the remote_server cross-build helpers embedded in web/build.sh against stub rustup/cargo.
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
build_script="${script_dir}/../build.sh"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

for function_name in host_triple triple_platform remote_server_version strip_debug_info require_static_linux_binary build_remote_server cross_subcommand build_remote_servers; do
    sed -n "/^${function_name}() {\$/,/^}\$/p" "${build_script}" >>"${work}/functions.sh"
done
if ! grep -q '^cross_subcommand() {$' "${work}/functions.sh" ||
    ! grep -q '^build_remote_server() {$' "${work}/functions.sh" ||
    ! grep -q '^build_remote_servers() {$' "${work}/functions.sh"; then
    echo "could not extract the helpers from ${build_script}" >&2
    exit 1
fi

die() {
    printf '%s\n' "$@" >&2
    exit 1
}

stubs="${work}/stubs"
mkdir -p "${stubs}"
cat >"${stubs}/rustup" <<'STUB'
#!/usr/bin/env bash
case "$1" in
    target)
        if [[ -n "${STUB_RUSTUP_TARGETS:-}" ]]; then
            printf '%s\n' "${STUB_RUSTUP_TARGETS}"
        else
            printf '%s\n' \
                x86_64-unknown-linux-gnu \
                aarch64-unknown-linux-gnu \
                x86_64-unknown-linux-musl
        fi
        ;;
    run)
        printf '%s' "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER-<unset>}" >"${STUB_RECORD}"
        printf '%s|%s' "${GITHUB_RUN_NUMBER-<unset>}" "${ZED_BUILD_ID-<unset>}" >"${STUB_BUILD_ID_RECORD}"
        if [[ "${3:-}" == rustc || "${3:-}" == */rustc ]]; then
            printf 'host: %s\n' "${STUB_HOST_TRIPLE:-x86_64-unknown-linux-gnu}"
            exit 0
        fi
        {
            printf 'RUSTFLAGS=%s ' "${RUSTFLAGS-<unset>}"
            printf 'CC_x86_64_unknown_linux_musl=%s ' "${CC_x86_64_unknown_linux_musl-<unset>}"
            printf 'CC_aarch64_unknown_linux_musl=%s ' "${CC_aarch64_unknown_linux_musl-<unset>}"
            printf 'args='
            printf '%q ' "$@"
            printf '\n'
        } >>"${STUB_CARGO_LOG}"
        target_dir=""
        target_triple=""
        previous=""
        for argument in "$@"; do
            if [[ "${previous}" == --target-dir ]]; then
                target_dir="${argument}"
            elif [[ "${previous}" == --target ]]; then
                target_triple="${argument}"
            fi
            previous="${argument}"
        done
        if [[ -n "${target_dir}" && -n "${STUB_OUTPUT_BINARY:-}" ]]; then
            if [[ -n "${target_triple}" ]]; then
                output="${target_dir}/${target_triple}/release/remote_server"
            else
                output="${target_dir}/release/remote_server"
            fi
            mkdir -p "$(dirname "${output}")"
            cp "${STUB_OUTPUT_BINARY}" "${output}"
            chmod +x "${output}"
        fi
        ;;
esac
STUB
cat >"${stubs}/cargo" <<'STUB'
#!/usr/bin/env bash
exit 1
STUB
cat >"${stubs}/aarch64-linux-gnu-gcc" <<'STUB'
#!/usr/bin/env bash
exit 0
STUB
cat >"${stubs}/strip" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${STUB_STRIP_LOG}"
exit 0
STUB
cat >"${stubs}/x86_64-linux-gnu-strip" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${STUB_STRIP_LOG}"
exit 0
STUB
chmod +x "${stubs}/rustup" "${stubs}/cargo" "${stubs}/aarch64-linux-gnu-gcc" \
    "${stubs}/strip" "${stubs}/x86_64-linux-gnu-strip"

export PATH="${stubs}:/usr/bin:/bin"
export STUB_RECORD="${work}/linker"
export STUB_BUILD_ID_RECORD="${work}/build-id"
export STUB_CARGO_LOG="${work}/cargo-log"
export STUB_STRIP_LOG="${work}/strip-log"
: >"${STUB_CARGO_LOG}"
: >"${STUB_STRIP_LOG}"
unset CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER
# Read by the functions sourced below from build.sh. shellcheck cannot follow that source.
# shellcheck disable=SC2034
repo_dir="${work}"
# shellcheck disable=SC2034
native_target="${work}/target"
# shellcheck disable=SC2034
stable_toolchain="stable"
# shellcheck disable=SC2034
cargo_bin="${stubs}/cargo"
# shellcheck disable=SC2034
remote_server_commit_sha="abc123"
# shellcheck source=/dev/null
source "${work}/functions.sh"

failures=0
expect() {
    local name="$1" expected="$2" actual="$3"
    if [[ "${actual}" == "${expected}" ]]; then
        echo "ok   ${name}"
    else
        echo "FAIL ${name}"
        echo "       expected: ${expected}"
        echo "       actual:   ${actual}"
        failures=$((failures + 1))
    fi
}

native=x86_64-unknown-linux-gnu
cross=aarch64-unknown-linux-gnu

gnu_cross_actual="skipped"
if gnu_cross_selected="$(cross_subcommand "${cross}")"; then
    gnu_cross_actual="${gnu_cross_selected}"
fi
expect "a glibc linux-gnu cross triple is not selected" "skipped" "${gnu_cross_actual}"

rm -f "${STUB_RECORD}"
build_remote_server "${cross}" build "${native}" >/dev/null
expect "a linux-gnu gcc is not injected as the linker" \
    "<unset>" "$(cat "${STUB_RECORD}" 2>/dev/null)"

rm -f "${STUB_RECORD}"
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=/opt/cross/bin/my-linker \
    build_remote_server "${cross}" build "${native}" >/dev/null
expect "a linker the user already set is kept" \
    "/opt/cross/bin/my-linker" "$(cat "${STUB_RECORD}" 2>/dev/null)"

rm -f "${STUB_RECORD}"
build_remote_server "${cross}" zigbuild "${native}" >/dev/null
expect "zigbuild brings its own linker" "<unset>" "$(cat "${STUB_RECORD}" 2>/dev/null)"

rm -f "${STUB_RECORD}"
build_remote_server "${native}" build "${native}" >/dev/null
expect "the native build gets no cross linker" "<unset>" "$(cat "${STUB_RECORD}" 2>/dev/null)"

# A CI build id becomes a prefix of `remote_server version`, and the browser compares that
# output with the manifest's plain commit.
for triple in "${native}" "${cross}"; do
    rm -f "${STUB_BUILD_ID_RECORD}"
    GITHUB_RUN_NUMBER=42 ZED_BUILD_ID=ci-7 build_remote_server "${triple}" build "${native}" >/dev/null
    expect "a CI build id does not reach the ${triple} remote_server build" \
        "<unset>|<unset>" "$(cat "${STUB_BUILD_ID_RECORD}" 2>/dev/null)"
done

export GITHUB_RUN_NUMBER=42 ZED_BUILD_ID=ci-7
build_remote_server "${native}" build "${native}" >/dev/null
expect "the remote_server build leaves the caller's build id alone" \
    "42|ci-7" "${GITHUB_RUN_NUMBER}|${ZED_BUILD_ID}"
unset GITHUB_RUN_NUMBER ZED_BUILD_ID

# A glibc-linked stand-in: it runs here, prints a commit, and ldd lists libc.
dynamic_server="${work}/dynamic-server"
if ! gcc -o "${dynamic_server}" -x c - <<'EOF'
#include <stdio.h>
int main(void) {
    puts("abc123");
    return 0;
}
EOF
then
    echo "FAIL could not compile a glibc-dynamic stand-in with gcc" >&2
    failures=$((failures + 1))
else
    export STUB_OUTPUT_BINARY="${dynamic_server}"
    # shellcheck disable=SC2034
    dist_dir="${work}/dist-linux"
    # shellcheck disable=SC2034
    import_remote_server="${script_dir}/import-remote-server.sh"
    : >"${STUB_CARGO_LOG}"
    linux_status=0
    (
        build_remote_servers
    ) >"${work}/brs-linux.out" 2>"${work}/brs-linux.err" || linux_status=$?

    musl_line="$(grep -F -- '--target x86_64-unknown-linux-musl' "${STUB_CARGO_LOG}" | head -n 1 || true)"
    musl_expected="RUSTFLAGS=-C target-feature=+crt-static CC_x86_64_unknown_linux_musl=musl-gcc --target x86_64-unknown-linux-musl"
    if [[ "${musl_line}" == *"RUSTFLAGS=-C target-feature=+crt-static"* &&
        "${musl_line}" == *"CC_x86_64_unknown_linux_musl=musl-gcc"* &&
        "${musl_line}" == *"--target x86_64-unknown-linux-musl"* ]]; then
        musl_actual="${musl_expected}"
    else
        musl_actual="${musl_line:-<no musl invocation>}"
    fi
    expect "native linux build uses --target x86_64-unknown-linux-musl with +crt-static and CC_x86_64_unknown_linux_musl=musl-gcc" \
        "${musl_expected}" "${musl_actual}"

    if [[ "${linux_status}" -ne 0 ]] &&
        grep -qiE 'static|dynamically linked|glibc' "${work}/brs-linux.err"; then
        dynamic_actual="refused"
    else
        dynamic_actual="accepted (status ${linux_status})"
    fi
    expect "a glibc-dynamic binary is refused" "refused" "${dynamic_actual}"
fi

: >"${STUB_CARGO_LOG}"
: >"${STUB_STRIP_LOG}"
mac_status=0
(
    export STUB_HOST_TRIPLE="aarch64-apple-darwin"
    STUB_RUSTUP_TARGETS="$(printf '%s\n' aarch64-apple-darwin x86_64-apple-darwin)"
    export STUB_RUSTUP_TARGETS
    # shellcheck disable=SC2034
    dist_dir="${work}/dist-mac"
    # shellcheck disable=SC2034
    import_remote_server="${script_dir}/import-remote-server.sh"
    build_remote_servers
) >"${work}/brs-mac.out" 2>"${work}/brs-mac.err" || mac_status=$?
apple_line="$(grep -F -- '--target x86_64-apple-darwin' "${STUB_CARGO_LOG}" | head -n 1 || true)"
apple_expected="build --target x86_64-apple-darwin"
if [[ "${apple_line}" == *" build "* && "${apple_line}" == *"--target x86_64-apple-darwin"* &&
    "${apple_line}" != *zigbuild* ]]; then
    apple_actual="${apple_expected}"
else
    apple_actual="${apple_line:-<other apple arch not built>} (status ${mac_status})"
fi
expect "macOS host builds the other apple arch when the target is installed" \
    "${apple_expected}" "${apple_actual}"
strip_s_count="$(grep -c -- '-S ' "${STUB_STRIP_LOG}" || true)"
expect "macOS strip -S runs for the native arch and the other arch" "2" "${strip_s_count}"

win_dist="${work}/win-dist"
win_binary="${work}/zed-remote-server.exe"
printf 'MZ abc123 windows' >"${win_binary}"
win_status=0
win_output="$(
    ZED_WEB_DIST_DIR="${win_dist}" "${script_dir}/import-remote-server.sh" \
        --commit abc123 "${win_binary}" windows x86_64 2>&1
)" || win_status=$?
win_exe="${win_dist}/bin/remote/zed-remote-server-windows-x86_64.exe"
win_manifest="${win_dist}/bin/remote/manifest.json"
if [[ "${win_status}" -eq 0 && -f "${win_exe}" && -f "${win_manifest}" ]] &&
    grep -q 'zed-remote-server-windows-x86_64.exe' "${win_manifest}" &&
    grep -q '"os": "windows"' "${win_manifest}"; then
    win_actual="exe-entry"
else
    win_actual="missing (status ${win_status}): ${win_output}"
fi
expect "import accepts windows and writes the .exe entry" "exe-entry" "${win_actual}"

# ---------------------------------------------------------------------------
# Regression tests for round 1 findings (SEC-BUILD-001 through SEC-BUILD-007)
# ---------------------------------------------------------------------------

# Test SEC-BUILD-001: import-remote-server.sh refuses glibc-dynamic Linux binary
import_dyn_dist="${work}/import-dyn-dist"
import_dyn_status=0
import_dyn_output="$(
    ZED_WEB_DIST_DIR="${import_dyn_dist}" "${script_dir}/import-remote-server.sh" \
        --commit abc123 "${dynamic_server}" linux x86_64 2>&1
)" || import_dyn_status=$?
if [[ "${import_dyn_status}" -ne 0 ]] && grep -qiE 'static|dynamically linked|glibc' <<<"${import_dyn_output}"; then
    import_dyn_actual="refused"
else
    import_dyn_actual="accepted (status ${import_dyn_status}): ${import_dyn_output}"
fi
expect "import-remote-server.sh refuses glibc-dynamic Linux binary" "refused" "${import_dyn_actual}"

# Test SEC-BUILD-002: build_remote_server returns .exe path for Windows targets
win_cargo_output="${work}/target/x86_64-pc-windows-gnu/release/remote_server.exe"
mkdir -p "$(dirname "${win_cargo_output}")"
touch "${win_cargo_output}"
win_built_binary="$(build_remote_server x86_64-pc-windows-gnu build "${native}")"
expect "build_remote_server returns .exe path for Windows target" \
    "${win_cargo_output}" "${win_built_binary}"

# Test SEC-BUILD-003: no Bash 3.2-incompatible local -a var=() in build.sh
b32_violations="$(grep -nE 'local -a [a-zA-Z0-9_]+=\(' "${build_script}" || true)"
expect "no Bash 3.2 incompatible local -a var=() in build.sh" \
    "" "${b32_violations}"

# Test SEC-BUILD-004: require_static_linux_binary succeeds without ldd when readelf confirms static ELF
static_server="${work}/static-server"
musl-gcc -static -x c -o "${static_server}" - <<<"int main(void){puts(\"abc123\");return 0;}" 2>/dev/null ||
gcc -static -x c -o "${static_server}" - <<<"int main(void){puts(\"abc123\");return 0;}"
no_ldd_status=0
no_ldd_err="$(
    (
        fake_bin="${work}/fake-no-ldd"
        mkdir -p "${fake_bin}"
        ln -s "$(command -v readelf)" "${fake_bin}/readelf"
        PATH="${fake_bin}:/bin:/usr/bin"
        require_static_linux_binary "${static_server}"
    ) 2>&1
)" || no_ldd_status=$?
if [[ "${no_ldd_status}" -eq 0 ]]; then
    no_ldd_actual="accepted"
else
    no_ldd_actual="failed (status ${no_ldd_status}): ${no_ldd_err}"
fi
expect "require_static_linux_binary succeeds when ldd is missing but readelf is available" \
    "accepted" "${no_ldd_actual}"

# Test SEC-BUILD-005: cross_subcommand rejects *-apple-darwin on Linux host
darwin_on_linux_status=0
darwin_on_linux_actual="selected"
(
    # shellcheck disable=SC2329
    host_triple() { echo "x86_64-unknown-linux-gnu"; }
    STUB_RUSTUP_TARGETS="$(printf '%s\n' aarch64-apple-darwin x86_64-apple-darwin)"
    export STUB_RUSTUP_TARGETS
    if ! cross_subcommand "aarch64-apple-darwin" >/dev/null 2>&1; then
        exit 1
    fi
) || darwin_on_linux_status=$?
if [[ "${darwin_on_linux_status}" -ne 0 ]]; then
    darwin_on_linux_actual="rejected"
fi
expect "cross_subcommand rejects *-apple-darwin on Linux host" \
    "rejected" "${darwin_on_linux_actual}"

# Test SEC-BUILD-006: import-remote-server.sh cleans up .partial files on failure
fail_dist="${work}/fail-dist"
mkdir -p "${fail_dist}/bin/remote"
fail_bin="${work}/fail-bin.exe"
printf 'MZ abc123 windows' >"${fail_bin}"
mkdir -p "${fail_dist}/bin/remote/manifest.json"
ZED_WEB_DIST_DIR="${fail_dist}" "${script_dir}/import-remote-server.sh" \
    --commit abc123 "${fail_bin}" windows x86_64 >/dev/null 2>&1 || true
leftover_partials="$(find "${fail_dist}" -name '*.partial' 2>/dev/null || true)"
expect "import-remote-server.sh leaves no .partial files on failure" \
    "" "${leftover_partials}"

# Test SEC-BUILD-007: strip_debug_info preserves .exe filename on Windows binary
win_to_strip="${work}/win-strip-src/remote_server.exe"
mkdir -p "$(dirname "${win_to_strip}")"
printf 'MZ test exe' >"${win_to_strip}"
stripped_win_binary="$(strip_debug_info "x86_64-pc-windows-gnu" "${native}" "${win_to_strip}")"
expect "strip_debug_info preserves .exe extension" \
    "${work}/target/remote-server-stripped/x86_64-pc-windows-gnu/remote_server.exe" \
    "${stripped_win_binary}"

# Test SEC-BUILD-004 (strictly isolated PATH without ldd):
no_ldd_isolated_status=0
no_ldd_isolated_err="$(
    (
        fake_bin_isolated="${work}/fake-no-ldd-isolated"
        mkdir -p "${fake_bin_isolated}"
        ln -s "$(command -v readelf)" "${fake_bin_isolated}/readelf"
        ln -s "$(command -v grep)" "${fake_bin_isolated}/grep"
        PATH="${fake_bin_isolated}"
        require_static_linux_binary "${static_server}"
    ) 2>&1
)" || no_ldd_isolated_status=$?
if [[ "${no_ldd_isolated_status}" -eq 0 ]]; then
    no_ldd_isolated_actual="accepted"
else
    no_ldd_isolated_actual="failed: ${no_ldd_isolated_err}"
fi
expect "require_static_linux_binary works with strictly isolated PATH without ldd" \
    "accepted" "${no_ldd_isolated_actual}"

[[ "${failures}" -eq 0 ]]
