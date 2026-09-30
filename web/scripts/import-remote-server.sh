#!/usr/bin/env bash
# Adds one remote_server binary to web/dist/bin/remote/ and records it in manifest.json.
#
# web/build.sh builds the host it can (static linux musl, the other apple arch when the
# rustup target is installed, Windows only when zig or mingw is already present). A binary
# from somewhere else is added here:
#   cargo build --release -p remote_server          # on the Mac, at the same commit
#   web/scripts/import-remote-server.sh target/release/remote_server macos aarch64
#   web/scripts/import-remote-server.sh --commit <sha> remote_server.exe windows x86_64
#
# Usage: import-remote-server.sh [--commit <version>] <binary> <os> <arch>
#   os is linux, macos, or windows; arch is x86_64 or aarch64.
#   Windows files are stored as zed-remote-server-windows-<arch>.exe.
#   The commit is what `<binary> version` prints (last non-empty line). --commit is for a
#   binary this machine cannot run, such as a macOS binary on Linux or a Windows .exe.
#   The file must still contain that commit string (the sha after a build-id prefix).
# Environment: ZED_WEB_DIST_DIR overrides the dist directory (default: web/dist).
set -euo pipefail

die() {
    printf '%s\n' "$@" >&2
    exit 1
}

usage() {
    die "usage: import-remote-server.sh [--commit <version>] <binary> <os> <arch>" \
        "  os: linux | macos | windows    arch: x86_64 | aarch64" \
        "  Windows binaries are stored as zed-remote-server-windows-<arch>.exe." \
        "  A binary this machine cannot run (macOS on Linux, or a Windows .exe) needs" \
        "  --commit <version>. The file must contain that commit (the sha after a '+' build id)."
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dist_dir="${ZED_WEB_DIST_DIR:-${script_dir}/../dist}"

commit=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --commit)
            [[ $# -ge 2 ]] || usage
            commit="$2"
            shift 2
            ;;
        --)
            shift
            break
            ;;
        -*)
            usage
            ;;
        *)
            break
            ;;
    esac
done
[[ $# -eq 3 ]] || usage
binary="$1"
os="$2"
arch="$3"

case "${os}" in
    linux | macos | windows) ;;
    *) die "error: unknown os '${os}' (expected linux, macos, or windows)." ;;
esac
case "${arch}" in
    x86_64 | aarch64) ;;
    *) die "error: unknown arch '${arch}' (expected x86_64 or aarch64)." ;;
esac
[[ -f "${binary}" ]] || die "error: ${binary} is not a file."

# A linux remote_server must be a static musl binary. A glibc-linked binary fails on older
# remotes (GLIBC_x.xx not found).
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

    if [[ -n "${readelf_bin}" ]] && "${readelf_bin}" -h "${binary}" >/dev/null 2>&1; then
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
        return 0
    fi

    if ! command -v ldd >/dev/null 2>&1 && [[ -z "${readelf_bin}" ]]; then
        if [[ "$(uname -s)" == "Linux" ]]; then
            die "error: neither readelf nor ldd was found, so ${binary} cannot be checked for glibc linkage."
        fi
    fi
}

if [[ "${os}" == "linux" ]]; then
    require_static_linux_binary "${binary}"
fi

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

if [[ -z "${commit}" ]]; then
    if [[ ! -x "${binary}" ]]; then
        die "error: ${binary} is not executable, so its commit cannot be read from '<binary> version'." \
            "Pass --commit <version> if this machine cannot run it (a macOS binary on Linux, or a Windows .exe)."
    fi
    if ! version_output="$("${binary}" version 2>/dev/null)"; then
        die "error: '${binary} version' failed, so the commit cannot be read." \
            "Pass --commit <version> if this machine cannot run the binary (a macOS binary on Linux, or a Windows .exe)."
    fi
    # The last non-empty line: a shell profile on the machine may print noise first.
    commit="$(printf '%s\n' "${version_output}" | awk 'NF { line = $0 } END { print line }')"
    commit="$(printf '%s' "${commit}" | tr -d '[:space:]')"
fi
if [[ ! "${commit}" =~ ^[0-9A-Za-z._+-]+$ ]]; then
    die "error: the commit '${commit}' is empty or contains characters other than [0-9A-Za-z._+-]."
fi
# remote_server embeds its commit as a string literal, so a --commit for a binary this machine
# cannot run is still checked. With a build id, `version` prints `<id>+<sha>` and only the
# sha is a literal.
if ! LC_ALL=C grep -qaF -- "${commit##*+}" "${binary}"; then
    die "error: ${binary} does not contain the commit '${commit##*+}', so it was not built at '${commit}'." \
        "Pass the commit the binary was built at (what '<binary> version' prints on its own machine)."
fi

remote_dir="${dist_dir}/bin/remote"
manifest="${remote_dir}/manifest.json"
file_name="zed-remote-server-${os}-${arch}"
if [[ "${os}" == windows ]]; then
    file_name="${file_name}.exe"
fi
mkdir -p "${remote_dir}"

cleanup_partials() {
    rm -f "${remote_dir}/${file_name}.partial" "${manifest}.partial"
}
trap cleanup_partials EXIT

if [[ -e "${manifest}" && ! -f "${manifest}" ]]; then
    die "error: ${manifest} exists but is not a regular file."
fi

# The manifest is rewritten in the layout below, one binary per line, so that this script
# and web/build.sh (which calls it) are the only writers and no JSON tool is needed.
entry_pattern='^    \{"os": "[a-z]+", "arch": "[a-z0-9_]+", "file": "[^"]+", "sha256": "[0-9a-f]{64}"\},?$'
kept_entries=()
if [[ -f "${manifest}" ]]; then
    existing_commit="$(sed -n 's/^  "commit": "\([^"]*\)",$/\1/p' "${manifest}")"
    [[ -n "${existing_commit}" ]] ||
        die "error: ${manifest} does not have the layout this script writes; delete it and re-run web/build.sh."
    listed="$(grep -c '"os":' "${manifest}" || true)"
    entries=()
    while IFS= read -r line; do
        entries+=("${line}")
    done < <(grep -E "${entry_pattern}" "${manifest}" || true)
    if [[ "${listed}" != "${#entries[@]}" ]]; then
        die "error: ${manifest} does not have the layout this script writes; delete it and re-run web/build.sh."
    fi
    for line in "${entries[@]+"${entries[@]}"}"; do
        line="${line%,}"
        if [[ "${line}" == *"\"os\": \"${os}\", \"arch\": \"${arch}\""* ]]; then
            continue
        fi
        kept_entries+=("${line}")
    done
    if [[ "${existing_commit}" != "${commit}" && "${#kept_entries[@]}" -gt 0 ]]; then
        die "error: ${manifest} is for commit ${existing_commit}, but this binary is ${commit}." \
            "Mixing commits would deploy servers that do not match each other." \
            "Rebuild every platform at one commit, or delete ${remote_dir} to start over."
    fi
fi

install -m 0755 "${binary}" "${remote_dir}/${file_name}.partial"
mv -f "${remote_dir}/${file_name}.partial" "${remote_dir}/${file_name}"
sha256="$(sha256_of "${remote_dir}/${file_name}")"
kept_entries+=("    {\"os\": \"${os}\", \"arch\": \"${arch}\", \"file\": \"${file_name}\", \"sha256\": \"${sha256}\"}")

sorted_entries=()
while IFS= read -r line; do
    sorted_entries+=("${line}")
done < <(printf '%s\n' "${kept_entries[@]}" | LC_ALL=C sort)

{
    printf '{\n  "commit": "%s",\n  "binaries": [\n' "${commit}"
    last=$((${#sorted_entries[@]} - 1))
    for i in "${!sorted_entries[@]}"; do
        if [[ "${i}" -lt "${last}" ]]; then
            printf '%s,\n' "${sorted_entries[i]}"
        else
            printf '%s\n' "${sorted_entries[i]}"
        fi
    done
    printf '  ]\n}\n'
} >"${manifest}.partial"
mv -f "${manifest}.partial" "${manifest}"
trap - EXIT

printf 'Imported %s-%s (commit %s, sha256 %s) into %s\n' "${os}" "${arch}" "${commit}" "${sha256}" "${remote_dir}"
