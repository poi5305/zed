#!/usr/bin/env bash
# Exercises web/build.sh's font download helpers against a fake curl. Only the two helper
# functions are lifted out of build.sh, so nothing is downloaded and no build runs.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
build_script="${script_dir}/../build.sh"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

helpers="${work}/helpers.sh"
{
    printf '%s\n' 'die() { printf "%s\n" "$@" >&2; exit 1; }'
    sed -n '/^sha256_of() {/,/^}/p' "${build_script}"
    sed -n '/^ensure_font() {/,/^}/p' "${build_script}"
} > "${helpers}"
if ! grep -q '^ensure_font() {' "${helpers}" || ! grep -q '^sha256_of() {' "${helpers}"; then
    echo "FAIL: could not lift sha256_of/ensure_font out of ${build_script}" >&2
    exit 1
fi

fake_bin="${work}/bin"
mkdir -p "${fake_bin}"
# Writes some bytes to the -o target and then dies, like a connection reset mid-body (curl 56).
cat > "${fake_bin}/curl" <<'EOF'
#!/usr/bin/env bash
output=""
while [[ $# -gt 0 ]]; do
    if [[ "$1" == "-o" ]]; then
        output="$2"
        shift
    fi
    shift
done
printf 'half a fon' > "${output}"
exit 56
EOF
chmod +x "${fake_bin}/curl"

failures=0
expect_no_leftovers() {
    local case_name="$1" dir="$2" leftovers
    leftovers="$(find "${dir}" -name '*.partial' -print)"
    if [[ -n "${leftovers}" ]]; then
        echo "FAIL ${case_name}: left behind ${leftovers}" >&2
        failures=$((failures + 1))
    else
        echo "ok   ${case_name}: no .partial left"
    fi
}

run_ensure_font() {
    local dir="$1" expected="$2"
    (
        set -euo pipefail
        export PATH="${fake_bin}:${PATH}"
        # shellcheck disable=SC1090
        source "${helpers}"
        ensure_font "Test Font" "${dir}" Test-Regular.ttf "https://example.invalid/t.ttf" \
            "${expected}" "https://example.invalid/OFL.txt"
    ) > "${work}/last.log" 2>&1 || true
}

# 1. curl dies after writing part of the body.
dir="${work}/case-curl-fails"
run_ensure_font "${dir}" "0000000000000000000000000000000000000000000000000000000000000000"
expect_no_leftovers "a failed download" "${dir}"

# 2. An earlier interrupted run left a .partial beside a font that is already valid. The valid
#    font is skipped, but build.sh tars the whole directory, so the leftover would be shipped.
dir="${work}/case-stale-partial"
mkdir -p "${dir}"
printf 'valid font bytes' > "${dir}/Test-Regular.ttf"
printf 'OFL' > "${dir}/OFL.txt"
printf 'half a fon' > "${dir}/Test-Regular.ttf.partial"
valid_sha="$(source "${helpers}" && sha256_of "${dir}/Test-Regular.ttf")"
run_ensure_font "${dir}" "${valid_sha}"
expect_no_leftovers "a stale .partial beside a valid font" "${dir}"

# 3. The license download dies mid-body; a truncated OFL.txt must not be kept as if complete.
dir="${work}/case-truncated-license"
mkdir -p "${dir}"
printf 'valid font bytes' > "${dir}/Test-Regular.ttf"
valid_sha="$(source "${helpers}" && sha256_of "${dir}/Test-Regular.ttf")"
run_ensure_font "${dir}" "${valid_sha}"
if [[ -e "${dir}/OFL.txt" ]]; then
    echo "FAIL a failed license download: kept a truncated OFL.txt ($(cat "${dir}/OFL.txt"))" >&2
    failures=$((failures + 1))
else
    echo "ok   a failed license download: no truncated OFL.txt kept"
fi

# 4. A checksum mismatch still fails loudly and leaves no .partial.
dir="${work}/case-mismatch"
mkdir -p "${dir}"
cat > "${fake_bin}/curl" <<'EOF'
#!/usr/bin/env bash
output=""
while [[ $# -gt 0 ]]; do
    if [[ "$1" == "-o" ]]; then
        output="$2"
        shift
    fi
    shift
done
printf 'wrong bytes' > "${output}"
EOF
run_ensure_font "${dir}" "0000000000000000000000000000000000000000000000000000000000000000"
if ! grep -q 'checksum mismatch' "${work}/last.log"; then
    echo "FAIL a checksum mismatch: no 'checksum mismatch' error printed" >&2
    failures=$((failures + 1))
else
    echo "ok   a checksum mismatch is reported"
fi
expect_no_leftovers "a checksum mismatch" "${dir}"

if [[ "${failures}" -ne 0 ]]; then
    echo "${failures} check(s) failed" >&2
    exit 1
fi
echo "all font helper checks passed"
