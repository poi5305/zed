#!/usr/bin/env bash

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
allowlist="$repo_root/web/wasm-std-systemtime.allowlist"
expected=$(mktemp)
actual=$(mktemp)
trap 'rm -f "$expected" "$actual"' EXIT

# Sibling of check-wasm-time.sh, for the other half of the same defect class.
# That gate only ever looked at Instant, so std::time::SystemTime::now() sat in
# crates/claude_sessions/src/claude_sessions_panel.rs while it reported ok, and
# clicking the Claude Sessions panel aborted the browser app with
# "time not implemented on this platform" (library/std/src/sys/time/unsupported.rs:35).
# SystemTime::now() is unimplemented on wasm32-unknown-unknown exactly as
# Instant::now() is, so it needs the same inventory.
#
# Only now() panics: UNIX_EPOCH is a const and duration_since / checked_add /
# cmp are all implemented, so a std::time::SystemTime used purely as a type is
# legitimate. The gate therefore does not try to decide reachability itself --
# it diffs a normalized inventory against an allowlist whose entries carry a
# written reason, so a new occurrence has to be justified by a human rather
# than silently absorbed.
#
# The web workspace's manifest, not the root's: `zed_web_workspace` is a member
# of it, and the root excludes `web/`, so resolving the package from the root
# fails outright rather than checking anything.
cargo tree \
    --manifest-path "$repo_root/web/Cargo.toml" \
    -p zed_web_workspace \
    --target wasm32-unknown-unknown \
    -e normal \
    --prefix none \
    --format '{p}' |
    sed -n "s|.*(\\($repo_root/crates/[^)]*\\)).*|\\1|p" |
    sort -u |
    while IFS= read -r crate_dir; do
        test -d "$crate_dir/src" || continue
        find "$crate_dir/src" -type f -name '*.rs' -print0 |
            while IFS= read -r -d '' source_file; do
                perl -0777 -ne '
                    while (/\buse\s+std(?:(?!;).)*?\bSystemTime\b(?:(?!;).)*?;/sg) {
                        $match = $&;
                        $match =~ s/\s+/ /g;
                        print "$ARGV:$match\n";
                    }
                    while (/\bstd::time::SystemTime\b/g) {
                        print "$ARGV:std::time::SystemTime\n";
                    }
                ' "$source_file"
            done
    done |
    sed "s|^$repo_root/||" |
    sort |
    uniq -c |
    sed -E 's/^ +//' >"$actual"

# Unlike check-wasm-time.sh's allowlist, this one carries a reason per entry.
# Comment and blank lines are stripped before the comparison.
grep -v -e '^[[:space:]]*#' -e '^[[:space:]]*$' "$allowlist" >"$expected" || true

if ! diff -u "$expected" "$actual"; then
    cat >&2 <<'EOF'

Unexpected std::time::SystemTime usage was found in the web dependency graph.
It compiles for wasm32-unknown-unknown but SystemTime::now() panics in browsers,
and a wasm panic aborts without unwinding, so it wedges the whole app rather
than failing the one call. Use web_time::SystemTime in runtime code. Add to the
allowlist only with a written reason proving the occurrence is a type-only use,
is excluded from wasm by cfg, is test/fixture-only, or is blocked by a consumer
type that demands std::time::SystemTime.
EOF
    exit 1
fi
