#!/usr/bin/env bash
# Every panel the web shell attaches must also have its crate's `init` called.
#
# WHY THIS EXISTS. A panel is wired into the browser shell in two independent
# places: one statement attaches it to a dock, another calls its crate's
# `init(cx)` to register the actions it answers to. Nothing type-checks the
# pairing -- both halves compile perfectly on their own -- and when the second
# half is missing the result is completely silent:
#
#   * the panel loads its data and attaches, logging success;
#   * the status bar draws its button with the right icon and tooltip;
#   * clicking the button dispatches `<crate>::ToggleFocus`;
#   * no node in the dispatch tree handles it, so gpui drops it without a word.
#
# That is what happened to `project_manager`, `tmux_sessions` and
# `claude_sessions` for the whole of phase 6: three panels that were reported by
# the user as "not working" were in fact working and simply could not be opened.
# The shell already carried a comment about the same trap for the agent panel
# ("Desktop registers these in zed.rs (not agent_ui::init), so the web shell
# must do" it too) and it still happened three more times.
#
# Same idea as check-one-sided-rpc.sh: the link between the two halves is a
# name, and nothing type-checks a name.
#
# SCOPE. This checks panels attached through a fully-qualified path,
# `<crate>::<Something>Panel::load(...)`, which is how this fork's own panels are
# attached. Upstream Zed's panels are imported unqualified and are not covered.
#
# Usage: check-panel-actions.sh [shell-source.rs] [crates-dir]

set -euo pipefail

web_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd "${web_dir}/.." && pwd)"

shell_source="${1:-${web_dir}/crates/zed_web_workspace/src/main.rs}"
crates_dir="${2:-${repo_dir}/crates}"

die() {
    printf '%s\n' "$@" >&2
    exit 1
}

[[ -f "${shell_source}" ]] || die "error: ${shell_source} does not exist."
[[ -d "${crates_dir}" ]] || die "error: ${crates_dir} does not exist."

# The crate root of `<crate>`, honouring a `[lib] path = "..."` in its manifest
# (this repo's convention is a descriptive root, not lib.rs). Empty when the
# crate is not there at all.
crate_root() {
    local crate="$1"
    local manifest="${crates_dir}/${crate}/Cargo.toml"
    local candidate

    if [[ -f "${manifest}" ]]; then
        candidate="$(
            awk '/^\[lib\]/ { in_lib = 1; next }
                 /^\[/ { in_lib = 0 }
                 in_lib && /^[[:space:]]*path[[:space:]]*=/ {
                     sub(/^[^=]*=[[:space:]]*"/, "")
                     sub(/".*$/, "")
                     print
                     exit
                 }' "${manifest}"
        )"
        if [[ -n "${candidate}" && -f "${crates_dir}/${crate}/${candidate}" ]]; then
            printf '%s' "${crates_dir}/${crate}/${candidate}"
            return
        fi
    fi
    for candidate in "src/${crate}.rs" "src/lib.rs"; do
        if [[ -f "${crates_dir}/${crate}/${candidate}" ]]; then
            printf '%s' "${crates_dir}/${crate}/${candidate}"
            return
        fi
    done
}

attached_crates="$(
    grep -oE '\b[a-z_][a-z0-9_]*::[A-Za-z0-9_]*Panel::load[[:space:]]*\(' "${shell_source}" |
        sed 's/::.*//' |
        sort -u
)"

if [[ -z "${attached_crates}" ]]; then
    die "error: ${shell_source} attaches no fully-qualified <crate>::<X>Panel::load(...)." \
        "Either the attach site moved or this check's pattern is stale; it must not pass by finding nothing."
fi

missing=()
checked=0
skipped=()

while read -r crate; do
    [[ -n "${crate}" ]] || continue
    root="$(crate_root "${crate}")"
    if [[ -z "${root}" ]]; then
        die "error: cannot find the crate root of '${crate}' under ${crates_dir}." \
            "This check has to read it to know whether the crate has an \`init\` to call."
    fi
    # A crate with no `init` has nothing to register, so requiring a call to one
    # would reject a legitimate panel.
    if ! grep -qE '^[[:space:]]*pub fn init[[:space:]]*\(' "${root}"; then
        skipped+=("${crate} (no \`pub fn init\` in ${root#"${repo_dir}"/})")
        continue
    fi
    checked=$((checked + 1))
    if ! grep -qE "^[[:space:]]*${crate}::init\(" "${shell_source}"; then
        missing+=("${crate}")
    fi
done <<<"${attached_crates}"

for note in "${skipped[@]+"${skipped[@]}"}"; do
    printf 'note: not checked: %s\n' "${note}" >&2
done

if ((${#missing[@]} > 0)); then
    printf 'error: %s attaches these panels but never calls their crate init:\n' \
        "${shell_source#"${repo_dir}"/}" >&2
    for crate in "${missing[@]}"; do
        printf '  actual:   no `%s::init(cx);` line in the file\n' "${crate}" >&2
        printf '  expected: `%s::init(cx);` alongside the other crate inits\n' "${crate}" >&2
        printf '            (without it, %s::ToggleFocus has no handler and the\n' "${crate}" >&2
        printf '             status-bar button for its panel silently does nothing)\n' "${crate}" >&2
    done
    printf '%d of %d attached panel crates are missing their init.\n' \
        "${#missing[@]}" "${checked}" >&2
    exit 1
fi

printf 'ok: all %d attached panel crates call their init.\n' "${checked}"
