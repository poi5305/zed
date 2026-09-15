#!/usr/bin/env bash
# Asserts docs/web-zed-plan.md §9's "the desktop build is uncontaminated" invariants,
# plus the Phase 1 skeleton requirements that §3.2 states but nothing else enforces.
#
# Run from anywhere: ./web/check-workspace-isolation.sh
# Exits non-zero on the first failing invariant's count; every assertion runs.

set -uo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
web_dir="${repo_dir}/web"
# Never share target/ with the desktop build (§9's second standing rule).
probe_target_dir="${repo_dir}/target/web-probe"

failures=0
checks=0

# macOS has no coreutils `timeout`; perl's alarm is always present.
run_with_timeout() {
    local seconds="$1"
    shift
    perl -e 'alarm shift; exec @ARGV' "${seconds}" "$@"
}

pass() {
    checks=$((checks + 1))
    printf 'ok   %s\n' "$1"
}

fail() {
    checks=$((checks + 1))
    failures=$((failures + 1))
    printf 'FAIL %s\n' "$1"
    shift
    while [[ $# -gt 0 ]]; do
        printf '       %s\n' "$1"
        shift
    done
}

expect_equal() {
    local label="$1" expected="$2" actual="$3"
    if [[ "${expected}" == "${actual}" ]]; then
        pass "${label}"
    else
        fail "${label}" "expected: ${expected}" "actual:   ${actual}"
    fi
}

# ---------------------------------------------------------------------------
# §9 bullet 1 — the nine crates' source fields in the ROOT Cargo.lock
# ---------------------------------------------------------------------------
nine_expected=$(cat <<'EOF'
agent-client-protocol 2.0.0 registry+https://github.com/rust-lang/crates.io-index
alacritty_terminal 0.26.1-dev git+https://github.com/zed-industries/alacritty?rev=4c129667ce56611becdc82de6e28218c80e2e88f#4c129667ce56611becdc82de6e28218c80e2e88f
async-tar 0.6.1 git+https://github.com/zed-industries/async-tar?rev=bd3ad6f89df9a9da7a8535958756d6bf465936a0#bd3ad6f89df9a9da7a8535958756d6bf465936a0
lsp-types 0.95.1 git+https://github.com/zed-industries/lsp-types?rev=f1783e63a7f4eb4397bf51d4148b4895a1f7ab16#f1783e63a7f4eb4397bf51d4148b4895a1f7ab16
smol 2.0.2 registry+https://github.com/rust-lang/crates.io-index
tree-sitter 0.27.0 git+https://github.com/tree-sitter/tree-sitter?rev=43623ec9bf0eaaf7113285c46e8a09018f181b18#43623ec9bf0eaaf7113285c46e8a09018f181b18
url 2.5.7 registry+https://github.com/rust-lang/crates.io-index
wasm_thread 0.3.3 git+https://github.com/zed-industries/wasm_thread?rev=0cf96c7708dfb97ccf3da50347e25edcf75d6937#0cf96c7708dfb97ccf3da50347e25edcf75d6937
which 8.0.5 registry+https://github.com/rust-lang/crates.io-index
EOF
)
nine_actual=$(python3 - "${repo_dir}/Cargo.lock" <<'PY'
import re, sys
wanted = {
    "agent-client-protocol", "alacritty_terminal", "async-tar", "lsp-types",
    "smol", "tree-sitter", "url", "wasm_thread", "which",
}
rows = []
for block in open(sys.argv[1]).read().split("[[package]]")[1:]:
    name = re.search(r'^name = "(.*)"', block, re.M)
    if not name or name.group(1) not in wanted:
        continue
    version = re.search(r'^version = "(.*)"', block, re.M)
    source = re.search(r'^source = "(.*)"', block, re.M)
    rows.append(f'{name.group(1)} {version.group(1) if version else "<no version>"} '
               f'{source.group(1) if source else "<no source: path dependency>"}')
print("\n".join(sorted(rows)))
PY
)
if [[ "${nine_expected}" == "${nine_actual}" ]]; then
    pass "§9.1 the nine crates resolve to their recorded sources in the root Cargo.lock"
else
    fail "§9.1 the nine crates resolve to their recorded sources in the root Cargo.lock" \
        "diff (expected < / actual >):"
    diff <(printf '%s\n' "${nine_expected}") <(printf '%s\n' "${nine_actual}") \
        | sed 's/^/       /'
fi

# ---------------------------------------------------------------------------
# §9 bullet 2 — no web/ package in the root graph.
#
# This CANNOT be checked against Cargo.lock: a path dependency is recorded there
# with no `source` field and no path, so a crate living under web/ appears as a
# bare `[[package]] name = "..."` indistinguishable from any other. `exclude`
# stops membership, not graph inclusion. Ask cargo for manifest paths instead.
# ---------------------------------------------------------------------------
root_meta="$(run_with_timeout 300 env CARGO_TARGET_DIR="${probe_target_dir}" \
    cargo metadata --format-version 1 --no-deps --manifest-path "${repo_dir}/Cargo.toml" 2>/dev/null)"
if [[ -z "${root_meta}" ]]; then
    fail "§9.2 root workspace metadata is readable" \
        "expected: cargo metadata --no-deps to succeed for ${repo_dir}/Cargo.toml" \
        "actual:   empty output or timeout after 300s"
else
    web_packages=$(printf '%s' "${root_meta}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
hits = [p["manifest_path"] for p in d["packages"] if "/web/" in p["manifest_path"]]
print(",".join(hits) if hits else "none")')
    expect_equal "§9.2 no root-workspace package has a manifest under web/" "none" "${web_packages}"

    member_count=$(printf '%s' "${root_meta}" | python3 -c '
import json, sys
print(len(json.load(sys.stdin)["workspace_members"]))')
    expect_equal "§9.2 root workspace member count is unchanged" "257" "${member_count}"

    root_ws_root=$(printf '%s' "${root_meta}" | python3 -c '
import json, sys
print(json.load(sys.stdin)["workspace_root"])')
    expect_equal "§9.2 root workspace_root" "${repo_dir}" "${root_ws_root}"
fi

# ---------------------------------------------------------------------------
# §3.2 — web/ resolves to its own workspace and its own target directory
# ---------------------------------------------------------------------------
# CARGO_TARGET_DIR is unset on purpose: the invariant is where the WORKSPACE puts
# its artefacts, not where an ambient environment variable redirects them.
web_meta="$(cd "${web_dir}" && run_with_timeout 300 env -u CARGO_TARGET_DIR \
    cargo metadata --format-version 1 --no-deps 2>/dev/null)"
if [[ -z "${web_meta}" ]]; then
    fail "§3.2 web workspace metadata is readable" \
        "expected: cargo metadata --no-deps to succeed with cwd=${web_dir}" \
        "actual:   empty output or timeout after 300s"
else
    web_ws_root=$(printf '%s' "${web_meta}" | python3 -c '
import json, sys
print(json.load(sys.stdin)["workspace_root"])')
    expect_equal "§3.2 web workspace_root is web/, not the repo root" "${web_dir}" "${web_ws_root}"

    web_target=$(printf '%s' "${web_meta}" | python3 -c '
import json, sys
print(json.load(sys.stdin)["target_directory"])')
    expect_equal "§9 web build does not share target/ with the desktop build" \
        "${web_dir}/target" "${web_target}"
fi

# ---------------------------------------------------------------------------
# §3.2 — rustflags match the reference build, token for token
# ---------------------------------------------------------------------------
flags_expected='--cfg getrandom_backend="wasm_js" -C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--initial-memory=134217728 -C link-arg=--max-memory=4294967296 -C link-arg=--export=__heap_base -C link-arg=--export=__stack_pointer -C link-arg=--export=__tls_size -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__wasm_call_ctors'
flags_actual=$(python3 - "${web_dir}/.cargo/config.toml" <<'PY'
import sys, tomllib
try:
    cfg = tomllib.load(open(sys.argv[1], "rb"))
except FileNotFoundError:
    print("<web/.cargo/config.toml missing>")
    raise SystemExit
print(" ".join(cfg.get("target", {})
                  .get("wasm32-unknown-unknown", {})
                  .get("rustflags", ["<no rustflags>"])))
PY
)
expect_equal "§3.2 wasm rustflags equal zedweb/zed-web:web/build.sh:45" \
    "${flags_expected}" "${flags_actual}"

# ---------------------------------------------------------------------------
# F6 — nothing above web/ in the config discovery chain may redefine the wasm
# target (target.* entries CONCATENATE), and no environment override may be in
# scope (RUSTFLAGS and CARGO_ENCODED_RUSTFLAGS REPLACE the config entirely).
# ---------------------------------------------------------------------------
competing=""
for candidate in "${repo_dir}/.cargo/config.toml" "${HOME}/.cargo/config.toml" "${HOME}/.cargo/config"; do
    [[ -f "${candidate}" ]] || continue
    if python3 - "${candidate}" <<'PY'
import sys, tomllib
cfg = tomllib.load(open(sys.argv[1], "rb"))
raise SystemExit(0 if "wasm32-unknown-unknown" in cfg.get("target", {}) else 1)
PY
    then
        competing="${competing}${candidate} "
    fi
done
expect_equal "F6 no config above web/ redefines [target.wasm32-unknown-unknown]" \
    "none" "${competing:-none}"
expect_equal "F6 RUSTFLAGS is not set (it would replace the config rustflags)" \
    "<unset>" "${RUSTFLAGS:-<unset>}"
expect_equal "F6 CARGO_ENCODED_RUSTFLAGS is not set (it would replace the config rustflags)" \
    "<unset>" "${CARGO_ENCODED_RUSTFLAGS:-<unset>}"

# ---------------------------------------------------------------------------
# F1 — patches do not cross workspaces. Every root [patch.crates-io] entry that
# the wasm32 graph actually reaches must be repeated in web/Cargo.toml.
# ---------------------------------------------------------------------------
wasm_meta_file="$(mktemp -t zed-web-wasm-meta)"
trap 'rm -f "${wasm_meta_file}"' EXIT
run_with_timeout 300 env CARGO_TARGET_DIR="${probe_target_dir}" \
    cargo metadata --format-version 1 --filter-platform wasm32-unknown-unknown \
    --manifest-path "${repo_dir}/Cargo.toml" > "${wasm_meta_file}" 2>/dev/null
if [[ ! -s "${wasm_meta_file}" ]]; then
    fail "F1 wasm32-filtered root metadata is readable" \
        "expected: cargo metadata --filter-platform wasm32-unknown-unknown to succeed" \
        "actual:   empty output or timeout after 300s"
else
    missing_patches=$(python3 - \
        "${repo_dir}/Cargo.toml" "${web_dir}/Cargo.toml" "${wasm_meta_file}" <<'PY'
import collections, json, sys, tomllib

root_patch = set(tomllib.load(open(sys.argv[1], "rb"))
                 .get("patch", {}).get("crates-io", {}))
web_patch = set(tomllib.load(open(sys.argv[2], "rb"))
                .get("patch", {}).get("crates-io", {}))

metadata = json.load(open(sys.argv[3]))
nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
by_id = {package["id"]: package for package in metadata["packages"]}
name_to_ids = collections.defaultdict(list)
for package in metadata["packages"]:
    name_to_ids[package["name"]].append(package["id"])

# Crates zed-web proves are in the wasm graph (plan §5.5, §6.4.1).
anchors = ["gpui", "scheduler", "rpc", "remote", "project", "editor"]
reachable = set()
stack = [package_id for anchor in anchors for package_id in name_to_ids[anchor]]
seen = set()
while stack:
    package_id = stack.pop()
    if package_id in seen:
        continue
    seen.add(package_id)
    reachable.add(by_id[package_id]["name"])
    for dependency in nodes.get(package_id, {}).get("deps", []):
        stack.append(dependency["pkg"])

missing = sorted((root_patch & reachable) - web_patch)
print(",".join(missing) if missing else "none")
PY
)
    expect_equal "F1 every wasm-reachable root [patch.crates-io] entry is repeated in web/Cargo.toml" \
        "none" "${missing_patches}"
fi

# ---------------------------------------------------------------------------
# F2 — [profile.*] is honoured only at a workspace root, so the profile the
# reference build uses must be re-declared in web/Cargo.toml.
# ---------------------------------------------------------------------------
profile_expected='codegen-units=1 debug=False inherits=release lto=thin opt-level=z strip=symbols'
profile_actual=$(python3 - "${web_dir}/Cargo.toml" <<'PY'
import sys, tomllib
profile = tomllib.load(open(sys.argv[1], "rb")).get("profile", {}).get("web-release")
if profile is None:
    print("<[profile.web-release] not declared>")
else:
    print(" ".join(f"{key}={value}" for key, value in sorted(profile.items())))
PY
)
expect_equal "F2 web/Cargo.toml declares [profile.web-release] as zedweb/zed-web:Cargo.toml:1099 does" \
    "${profile_expected}" "${profile_actual}"

# ---------------------------------------------------------------------------
# F4 — +atomics needs a std rebuilt with atomics (-Z build-std, nightly).
# Either the toolchain web/ resolves to can do that, or the gap is written down.
# ---------------------------------------------------------------------------
web_toolchain="$(cd "${web_dir}" && run_with_timeout 60 rustup show active-toolchain 2>/dev/null | head -n 1)"
if [[ "${web_toolchain}" == *nightly* ]]; then
    pass "F4 web/ resolves to a nightly toolchain, so -Z build-std is available (${web_toolchain})"
elif grep -q 'build-std' "${web_dir}/.cargo/config.toml" 2>/dev/null; then
    pass "F4 web/ cannot run -Z build-std yet, and the requirement is recorded in web/.cargo/config.toml"
else
    fail "F4 the -Z build-std / nightly requirement of +atomics is either satisfied or recorded" \
        "expected: a nightly toolchain for web/, or a 'build-std' note in web/.cargo/config.toml" \
        "actual:   toolchain '${web_toolchain:-<unknown>}' and no build-std note in web/.cargo/config.toml" \
        "see:      zedweb/zed-web:web/build.sh:53-58"
fi

# ---------------------------------------------------------------------------
# F3 — these rustflags are scoped to the CURRENT WORKING DIRECTORY, not to the
# workspace. Invoking the wasm build from the repo root with --manifest-path
# silently substitutes the desktop [build] rustflags. Measured, not inferred.
# The skeleton cannot prevent it, so it must at least say so.
# ---------------------------------------------------------------------------
if grep -q 'working directory' "${web_dir}/.cargo/config.toml" 2>/dev/null; then
    pass "F3 web/.cargo/config.toml records that it is discovered from the working directory"
else
    fail "F3 web/.cargo/config.toml records that it is discovered from the working directory" \
        "expected: a note explaining that cargo --manifest-path web/Cargo.toml run from the" \
        "          repo root reads none of these flags and applies the desktop ones instead" \
        "actual:   no such note"
fi

# ===========================================================================
# Phase 2 — vendored forks. §11's first-listed high risk is "a vendored fork
# silently rolls a dependency backwards": the version still resolves, the crate
# still compiles, the behaviour differs. Everything below turns that into a
# mechanical check against the crate each fork was cut from.
# ===========================================================================

vendor_dir="${web_dir}/vendor"
registry_dir="${HOME}/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f"
acp_base="${registry_dir}/agent-client-protocol-2.0.0"
url_base="${registry_dir}/url-2.5.7"
wasm_thread_base="${HOME}/.cargo/git/checkouts/wasm_thread-586cb4b723c583fe/0cf96c7"

# ---------------------------------------------------------------------------
# V1 — every vendored package keeps its base's name and version. A fork that
# renames or re-versions itself stops being patchable onto the same dependency.
# ---------------------------------------------------------------------------
versions_expected='agent-client-protocol 2.0.0
alacritty_terminal 0.26.1-dev
async-tar 0.6.1
smol 2.0.2
tree-sitter 0.27.0
url 2.5.7
wasm_thread 0.3.3'
versions_actual=$(python3 - "${vendor_dir}" <<'VPY'
import pathlib, sys, tomllib
rows = []
for manifest in sorted(pathlib.Path(sys.argv[1]).glob("*/Cargo.toml")):
    package = tomllib.load(open(manifest, "rb"))["package"]
    rows.append(f'{package["name"]} {package["version"]}')
print("\n".join(sorted(rows)))
VPY
)
expect_equal "V1 the seven vendored packages keep their base name and version" \
    "${versions_expected}" "${versions_actual}"

# ---------------------------------------------------------------------------
# V2 — agent_client_protocol_patch: §4 says src/ is byte-identical to crates.io
# 2.0.0 across 51 files and the only change is 4 cfg lines in lib.rs. Hash the
# 50 untouched files, and require lib.rs to differ by EXACTLY those 4 lines.
# ---------------------------------------------------------------------------
if [[ ! -d "${acp_base}/src" ]]; then
    fail "V2 agent_client_protocol_patch/src is byte-identical to crates.io 2.0.0 apart from 4 cfg lines in lib.rs" \
        "expected: the crates.io source of agent-client-protocol 2.0.0 to be unpacked" \
        "actual:   ${acp_base}/src does not exist (run: cargo fetch)"
else
    acp_report=$(python3 - "${acp_base}/src" "${vendor_dir}/agent_client_protocol_patch/src" <<'VPY'
import hashlib, pathlib, sys

def digest(root):
    root = pathlib.Path(root)
    files = sorted(p for p in root.rglob("*") if p.is_file() and p.name != "lib.rs")
    accumulator = hashlib.sha256()
    for path in files:
        accumulator.update(str(path.relative_to(root)).encode())
        accumulator.update(path.read_bytes())
    return len(files), accumulator.hexdigest()

base_count, base_hash = digest(sys.argv[1])
fork_count, fork_hash = digest(sys.argv[2])

# The four lines §4 authorises, and nothing else.
authorised = '#[cfg(not(target_family = "wasm"))]'
base_lib = (pathlib.Path(sys.argv[1]) / "lib.rs").read_text().splitlines()
fork_lib = (pathlib.Path(sys.argv[2]) / "lib.rs").read_text().splitlines()
added = [line for line in fork_lib if line.strip() == authorised]
stripped = [line for line in fork_lib if line.strip() != authorised]

print(f"files={fork_count} hash={fork_hash} added_cfg_lines={len(added)} "
      f"rest_of_lib_identical={'yes' if stripped == base_lib else 'no'}")
print(f"files={base_count} hash={base_hash} added_cfg_lines=4 "
      f"rest_of_lib_identical=yes")
VPY
)
    expect_equal "V2 agent_client_protocol_patch/src is byte-identical to crates.io 2.0.0 apart from 4 cfg lines in lib.rs" \
        "$(printf '%s' "${acp_report}" | sed -n 2p)" "$(printf '%s' "${acp_report}" | sed -n 1p)"
fi

# ---------------------------------------------------------------------------
# V3 — url_wasm: §4 warns the reference fork carried rustfmt noise. Quantify it:
# no file other than src/lib.rs may differ from crates.io url 2.5.7 at all.
# ---------------------------------------------------------------------------
if [[ ! -d "${url_base}" ]]; then
    fail "V3 url_wasm differs from crates.io url 2.5.7 in src/lib.rs only (no rustfmt noise)" \
        "expected: the crates.io source of url 2.5.7 to be unpacked" \
        "actual:   ${url_base} does not exist (run: cargo fetch)"
else
    url_differing=$(diff -rq -x '.cargo-ok' "${url_base}" "${vendor_dir}/url_wasm" 2>/dev/null \
        | sed -E 's#^Files .*/url_wasm/##; s# and .*$##; s# differ$##; s#^Only in .*: #MISSING_OR_EXTRA:#' \
        | sort | paste -sd, -)
    expect_equal "V3 url_wasm differs from crates.io url 2.5.7 in src/lib.rs only (no rustfmt noise)" \
        "src/lib.rs" "${url_differing:-<none>}"
fi

# ---------------------------------------------------------------------------
# V4 — url_wasm's wasm branch must be reachable ONLY on wasm32-unknown-unknown.
#
# url 2.5.7 already had real from_file_path/to_file_path implementations on
# wasm32-wasip1/p2 (target_os = "wasi", already in the outer any(...)) and on
# wasm32-unknown-emscripten (unix, likewise). A branch selector written as the
# bare `target_arch = "wasm32"` is true there too, and silently replaces those
# working implementations with the browser stub. That is the §11 regression, and
# it is invisible: same version, same name, same everything but behaviour.
#
# Branch selectors are the cfgs that mention target_arch = "wasm32" and do NOT
# mention unix/windows; the outer any(...) gates (which do) only decide whether
# the methods exist at all, and are correct as written.
# ---------------------------------------------------------------------------
cfg_verdict=$(python3 - "${vendor_dir}/url_wasm/src/lib.rs" <<'VPY'
import re, subprocess, sys

TARGETS = ["wasm32-unknown-unknown", "wasm32-wasip1", "wasm32-wasip2",
           "wasm32-unknown-emscripten"]

def cfg_of(target):
    out = subprocess.run(["rustc", "--print", "cfg", "--target", target],
                         capture_output=True, text=True)
    values = set()
    for line in out.stdout.split():
        if "=" in line:
            key, value = line.split("=", 1)
            values.add((key, value.strip('"')))
        else:
            values.add((line, None))
    return values

def extract(text):
    predicates = []
    for match in re.finditer(r"#\[cfg\(", text):
        start = match.end() - 1
        depth, index = 0, start
        while index < len(text):
            if text[index] == "(":
                depth += 1
            elif text[index] == ")":
                depth -= 1
                if depth == 0:
                    break
            index += 1
        predicates.append(" ".join(text[start + 1:index].split()))
    return predicates

def parse(predicate):
    tokens = re.findall(r'[A-Za-z_][A-Za-z0-9_]*|"[^"]*"|[(),=]', predicate)
    position = 0

    def node():
        nonlocal position
        head = tokens[position]
        position += 1
        if head in ("all", "any", "not"):
            assert tokens[position] == "("
            position += 1
            children = []
            while tokens[position] != ")":
                children.append(node())
                if tokens[position] == ",":
                    position += 1
            position += 1
            return (head, children)
        if position < len(tokens) and tokens[position] == "=":
            position += 1
            value = tokens[position].strip('"')
            position += 1
            return ("kv", head, value)
        return ("key", head)

    tree = node()
    assert position == len(tokens), f"trailing tokens in {predicate!r}"
    return tree


def evaluate(tree, values):
    kind = tree[0]
    if kind == "all":
        return all(evaluate(child, values) for child in tree[1])
    if kind == "any":
        return any(evaluate(child, values) for child in tree[1])
    if kind == "not":
        return not evaluate(tree[1][0], values)
    if kind == "kv":
        # `feature = "std"` is not a target predicate; url's std feature is default-on.
        return True if tree[1] == "feature" else (tree[1], tree[2]) in values
    return any(entry[0] == tree[1] for entry in values)


cfgs = {target: cfg_of(target) for target in TARGETS}
source = open(sys.argv[1]).read()
selectors = [p for p in extract(source)
             if 'target_arch = "wasm32"' in p
             and "unix" not in p and "windows" not in p]

if not selectors:
    print('no branch selector mentioning target_arch = "wasm32" found')
    raise SystemExit

problems = []
for predicate in selectors:
    positive = predicate[4:-1] if predicate.startswith("not(") else predicate
    tree = parse(positive)
    if not evaluate(tree, cfgs["wasm32-unknown-unknown"]):
        problems.append(f"[{positive}] is FALSE on wasm32-unknown-unknown")
    for target in TARGETS[1:]:
        if evaluate(tree, cfgs[target]):
            problems.append(f"[{positive}] is TRUE on {target}")
print("; ".join(sorted(set(problems))) if problems else "wasm32-unknown-unknown only")
VPY
)
expect_equal "V4 url_wasm's wasm branch selectors fire on wasm32-unknown-unknown only" \
    "wasm32-unknown-unknown only" "${cfg_verdict}"

# ---------------------------------------------------------------------------
# V5 — wasm_thread_patch: §4 says port the worker JS ONTO our existing fork, do
# not swap the fork out. Exactly four files may differ from git 0cf96c77, and
# the native re-export line must survive.
# ---------------------------------------------------------------------------
if [[ ! -d "${wasm_thread_base}" ]]; then
    fail "V5 wasm_thread_patch differs from git 0cf96c77 in the four §4 files only" \
        "expected: the git checkout of zed-industries/wasm_thread 0cf96c77 to exist" \
        "actual:   ${wasm_thread_base} does not exist (run: cargo fetch)"
else
    wasm_thread_differing=$(diff -rq -x '.git' -x '.cargo-ok' -x '.github' \
        -x 'rust-toolchain.toml' -x 'examples-wasm-pack' \
        "${wasm_thread_base}" "${vendor_dir}/wasm_thread_patch" 2>/dev/null \
        | sed -E 's#^Files .*/wasm_thread_patch/##; s# and .*$##; s# differ$##; s#^Only in .*: #MISSING_OR_EXTRA:#' \
        | sort | paste -sd, -)
    expect_equal "V5 wasm_thread_patch differs from git 0cf96c77 in the four §4 files only" \
        "src/lib.rs,src/wasm32/js/web_worker.js,src/wasm32/js/web_worker_module.js,src/wasm32/signal.rs" \
        "${wasm_thread_differing:-<none>}"

    native_reexport=$(grep '^pub use std::thread::' "${vendor_dir}/wasm_thread_patch/src/lib.rs")
    native_reexport_base=$(grep '^pub use std::thread::' "${wasm_thread_base}/src/lib.rs")
    expect_equal "V5 wasm_thread_patch keeps 0cf96c77's native 'pub use std::thread::…' verbatim" \
        "${native_reexport_base}" "${native_reexport}"
fi

# ---------------------------------------------------------------------------
# V6 — smol_wasm: §4 says the native path must `pub use` real smol rather than
# hand-copy upstream's spawn, as the reference fork did. Assert that the whole
# non-wasm half of the crate is that one re-export, of a real `smol` package.
# ---------------------------------------------------------------------------
smol_native=$(python3 - "${vendor_dir}/smol_wasm" <<'VPY'
import pathlib, sys, tomllib

root = pathlib.Path(sys.argv[1])
manifest = tomllib.load(open(root / "Cargo.toml", "rb"))
native = manifest.get("target", {}).get('cfg(not(target_family = "wasm"))', {}) \
                 .get("dependencies", {}).get("smol_real", {})

lines = (root / "src" / "lib.rs").read_text().splitlines()
native_code, index = [], 0
while index < len(lines):
    stripped = lines[index].strip()
    if stripped.startswith("#[cfg(target_family"):
        index += 1
        while index < len(lines) and not lines[index].strip():
            index += 1
        if index < len(lines) and lines[index].rstrip().endswith("{"):
            depth = 0
            while index < len(lines):
                depth += lines[index].count("{") - lines[index].count("}")
                index += 1
                if depth == 0:
                    break
        else:
            index += 1
        continue
    if stripped.startswith("#[cfg(not(target_family"):
        index += 1
        continue
    if stripped and not stripped.startswith("//"):
        native_code.append(stripped)
    index += 1

print(f'package={native.get("package")} '
      f'source={"git" if "git" in native else "other"} '
      f'native_code={native_code}')
VPY
)
expect_equal "V6 smol_wasm's native half is a re-export of real smol, not a hand copy" \
    "package=smol source=git native_code=['pub use smol_real::*;']" "${smol_native}"

# ---------------------------------------------------------------------------
# V7 — a git dependency inside a vendored crate must be pinned by `rev`.
# `tag` and `branch` are mutable: the code behind a fork could change with no
# file in this repo changing, and §9's lock check only covers the ROOT lock.
# The root manifest uses rev on 36 of its 37 git URLs and tag on none.
# ---------------------------------------------------------------------------
unpinned=$(python3 - "${vendor_dir}" <<'VPY'
import pathlib, sys, tomllib

def walk(table, name, out):
    for key, value in table.items():
        if not isinstance(value, dict):
            continue
        if "git" in value and "rev" not in value:
            pin = next((f"{k}={value[k]}" for k in ("tag", "branch") if k in value), "no pin")
            out.append(f"{name}:{key}({pin})")
        else:
            walk(value, name, out)

out = []
for manifest in sorted(pathlib.Path(sys.argv[1]).glob("*/Cargo.toml")):
    walk(tomllib.load(open(manifest, "rb")), manifest.parent.name, out)
print(",".join(out) if out else "none")
VPY
)
expect_equal "V7 every git dependency inside web/vendor is pinned by rev, not a mutable tag/branch" \
    "none" "${unpinned}"

# ---------------------------------------------------------------------------
# V8 — a vendored package must be patched on the source kind the ROOT workspace
# actually resolves it from, and must not also carry an inert patch of the other
# kind. Measured, not assumed: with only [patch.crates-io], a git-sourced
# wasm_thread resolves to the git source and cargo prints "patch was not used";
# with only the git-URL table it resolves to the vendored path. An inert entry
# that reads as load-bearing is how the real one gets deleted by mistake.
# ---------------------------------------------------------------------------
patch_placement=$(python3 - "${repo_dir}/Cargo.lock" "${web_dir}/Cargo.toml" "${vendor_dir}" <<'VPY'
import pathlib, re, sys, tomllib

lock = open(sys.argv[1]).read()
patch = tomllib.load(open(sys.argv[2], "rb")).get("patch", {})
crates_io = patch.get("crates-io", {})

problems = []
for manifest in sorted(pathlib.Path(sys.argv[3]).glob("*/Cargo.toml")):
    name = tomllib.load(open(manifest, "rb"))["package"]["name"]
    source = ""
    for block in lock.split("[[package]]")[1:]:
        if re.search(rf'^name = "{re.escape(name)}"$', block, re.M):
            found = re.search(r'^source = "(.*)"', block, re.M)
            source = found.group(1) if found else ""
            break
    if not source:
        problems.append(f"{name}: not in the root lock")
        continue
    if source.startswith("registry+"):
        if name not in crates_io:
            problems.append(f"{name}: root source is crates.io but no [patch.crates-io] entry")
        continue
    url = source[len("git+"):].split("?")[0].split("#")[0]
    if name not in patch.get(url, {}):
        problems.append(f'{name}: root source is git but no [patch."{url}"] entry')
    if name in crates_io:
        problems.append(f"{name}: root source is git, so its [patch.crates-io] entry is inert")
print("; ".join(problems) if problems else "none")
VPY
)
expect_equal "V8 each vendored package is patched on the source kind the root workspace resolves it from, with no inert twin" \
    "none" "${patch_placement}"

# ---------------------------------------------------------------------------
# V9 — §11 lists "Two Cargo.lock files drift / Mitigation: both committed". The
# web lock has to exist for that to mean anything. And because the vendored ACP
# source is byte-identical to the root's 2.0.0, it must be built by the same
# companion crates the desktop builds it with — otherwise "byte-identical" buys
# nothing.
# ---------------------------------------------------------------------------
if [[ ! -f "${web_dir}/Cargo.lock" ]]; then
    fail "V9 web/Cargo.lock exists, so the web graph is pinned at all" \
        "expected: ${web_dir}/Cargo.lock" \
        "actual:   absent — the web workspace re-resolves from scratch on every build"
    fail "V9 the vendored agent-client-protocol is built by the companion crates the desktop builds it with" \
        "expected: agent-client-protocol-derive/-schema at the root lock's versions" \
        "actual:   no web/Cargo.lock to compare"
else
    pass "V9 web/Cargo.lock exists, so the web graph is pinned at all"
    acp_companions=$(python3 - "${repo_dir}/Cargo.lock" "${web_dir}/Cargo.lock" <<'VPY'
import re, sys

def versions(path, wanted):
    found = {}
    for block in open(path).read().split("[[package]]")[1:]:
        name = re.search(r'^name = "(.*)"', block, re.M)
        if name and name.group(1) in wanted:
            found[name.group(1)] = re.search(r'^version = "(.*)"', block, re.M).group(1)
    return found

wanted = {"agent-client-protocol-derive", "agent-client-protocol-schema"}
root, web = versions(sys.argv[1], wanted), versions(sys.argv[2], wanted)
mismatched = [f"{name}: root={root.get(name, '-')} web={web.get(name, '-')}"
              for name in sorted(wanted) if root.get(name) != web.get(name)]
print("; ".join(mismatched) if mismatched else "none")
VPY
)
    expect_equal "V9 the vendored agent-client-protocol is built by the companion crates the desktop builds it with" \
        "none" "${acp_companions}"
fi

# ---------------------------------------------------------------------------
# L1 — §9.1: the two lock files must agree on every shared package.
#
# web/Cargo.lock was first generated from nothing, so cargo resolved everything to
# the newest compatible release while the root lock has been pinned over months.
# 445 of 1225 shared packages disagreed -- 36% of the shared graph compiled as
# different code on web than on the desktop. It surfaced as `merman` failing to
# build in web/ with an unresolved import, because root pinned merman/-core/-render
# all at 0.8.0-alpha.5 while web had the two siblings at alpha.6, which moved that
# API. A version-skew bug wearing a missing-import costume.
#
# The fix is to SEED the second lock from the first (cp Cargo.lock web/Cargo.lock,
# then let cargo metadata reconcile), not to resolve it independently. Every §9
# check guards the desktop from the web; this is the one guarding the web from
# drifting away from the desktop, which is the direction that actually bit.
# ---------------------------------------------------------------------------
lock_drift=$(python3 - "${repo_dir}/Cargo.lock" "${web_dir}/Cargo.lock" <<'PY_LOCK'
import re, sys

def versions(path):
    out = {}
    for block in open(path).read().split("[[package]]")[1:]:
        name = re.search(r'^name = "(.*)"', block, re.M)
        version = re.search(r'^version = "(.*)"', block, re.M)
        if name and version:
            out.setdefault(name.group(1), set()).add(version.group(1))
    return out

root, web = versions(sys.argv[1]), versions(sys.argv[2])
# A web package resolving to a SUBSET of root's versions is fine -- it simply does
# not need one of them. The invariant is the other direction: web must never use a
# version the root lock has not vetted.
# The livekit/webrtc media stack and the older prost/pbjson chain it drags in are
# native-only: nothing in zed_web_workspace's wasm graph reaches them, so the wasm
# build never compiles these and their version is immaterial to it. They are listed
# rather than silently skipped so that a NEW disagreement still fails this check.
NATIVE_ONLY_MEDIA_STACK = {
    "calloop", "diffy", "fixedbitset", "livekit-protocol", "pbjson", "pbjson-build",
    "pbjson-types", "petgraph", "prost", "prost-build", "prost-derive", "prost-types",
    "webrtc-sys", "webrtc-sys-build",
}
drift = sorted(
    n for n in set(root) & set(web)
    if not web[n] <= root[n] and n not in NATIVE_ONLY_MEDIA_STACK
)
if not drift:
    print("none")
else:
    print(f"{len(drift)} disagree: " + ",".join(drift[:8]) + ("..." if len(drift) > 8 else ""))
PY_LOCK
)
expect_equal "L1 no shared package resolves in web to a version the root lock lacks (§9.1)" \
    "none" "${lock_drift}"

# ---------------------------------------------------------------------------
# V10 — §4.1: smol_wasm's load-bearing job is dropping async-io/async-process on
# wasm, which is what takes errno/polling/rustix out of the graph with it.
#
# `blocking` was in this list until §4.3. It is reachable as
# languages -> async-fs -> blocking, and `crates/languages` declares
# `async-fs.workspace = true` ungated -- as does zedweb/zed-web's copy of that file,
# byte for byte, so it is in the reference implementation's wasm graph too. The list
# was validated in round 2 against a 183-package graph holding only the four vendored
# crates, and did not survive the graph becoming real in Phase 4b. Asserting on it
# would be asserting that we differ from the implementation we are porting.
#
# `polling` stays: it has a second path in, alacritty_terminal's direct dependency,
# closed by vendoring that crate with the §4 cfg port. Keep it asserted so the closure
# cannot silently regress.
# ---------------------------------------------------------------------------
web_wasm_meta_file="$(mktemp -t zed-web-vendor-wasm-meta)"
native_check_log="$(mktemp -t zed-web-native-check)"
wasm_check_log="$(mktemp -t zed-web-wasm-check)"
trap 'rm -f "${wasm_meta_file}" "${web_wasm_meta_file}" "${native_check_log}" "${wasm_check_log}"' EXIT

(cd "${web_dir}" && run_with_timeout 300 env CARGO_TARGET_DIR="${probe_target_dir}" \
    cargo metadata --format-version 1 --filter-platform wasm32-unknown-unknown) \
    > "${web_wasm_meta_file}" 2>/dev/null
if [[ ! -s "${web_wasm_meta_file}" ]]; then
    fail "V10 the web wasm32 graph is free of async-io/async-process/polling/errno/rustix (§4.1's wall)" \
        "expected: cargo metadata --filter-platform wasm32-unknown-unknown to succeed in web/" \
        "actual:   empty output or timeout after 300s"
else
    wall_breaches=$(python3 -c '
import json, sys
names = {p["name"] for p in json.load(open(sys.argv[1]))["packages"]}
blocked = sorted(names & {"async-io", "async-process", "polling", "errno", "rustix"})
print(",".join(blocked) if blocked else "none")' "${web_wasm_meta_file}")
    expect_equal "V10 the web wasm32 graph is free of async-io/async-process/polling/errno/rustix (§4.1's wall)" \
        "none" "${wall_breaches}"
fi

# ---------------------------------------------------------------------------
# V11 — the web workspace has to actually compile. Phase 2 validated it with
# `cargo metadata --no-deps`, which compiles nothing; --all-targets is what
# catches a vendored manifest that kept a target whose dev-dependency the
# crates.io normalisation stripped.
# ---------------------------------------------------------------------------
(cd "${web_dir}" && run_with_timeout 900 env CARGO_TARGET_DIR="${probe_target_dir}" \
    cargo check --workspace --all-targets) > "${native_check_log}" 2>&1
native_check_status=$?
if [[ "${native_check_status}" -eq 0 ]]; then
    pass "V11 cd web && cargo check --workspace --all-targets succeeds"
else
    fail "V11 cd web && cargo check --workspace --all-targets succeeds" \
        "expected: exit 0" \
        "actual:   exit ${native_check_status}" \
        "$(grep -m3 '^error' "${native_check_log}" | tr '\n' ' ')"
fi

(cd "${web_dir}" && run_with_timeout 900 env CARGO_TARGET_DIR="${probe_target_dir}" \
    cargo check --workspace --target wasm32-unknown-unknown) > "${wasm_check_log}" 2>&1
wasm_check_status=$?
if [[ "${wasm_check_status}" -eq 0 ]]; then
    pass "V11 cd web && cargo check --workspace --target wasm32-unknown-unknown succeeds"
else
    fail "V11 cd web && cargo check --workspace --target wasm32-unknown-unknown succeeds" \
        "expected: exit 0" \
        "actual:   exit ${wasm_check_status}" \
        "$(grep -m3 '^error' "${wasm_check_log}" | tr '\n' ' ')"
fi

# ===========================================================================
# Phase 3a — first-party manifests. The port's failure mode here is not a bad
# gate, it is a MISREAD one: "zed-web's manifest has X and ours does not" turned
# into "add X", when zed-web only has X because its SOURCES use it. Everything
# below compares the current manifests against the commit Phase 3a started
# from, so the assertions keep their meaning after the work is committed.
# ===========================================================================

phase3a_base="ee080f343354ad3a367e35bbd95132dea535c806"
if ! git -C "${repo_dir}" cat-file -e "${phase3a_base}^{commit}" 2>/dev/null; then
    fail "M0 the Phase 3a base commit is reachable" \
        "expected: ${phase3a_base} to exist in ${repo_dir}" \
        "actual:   git cat-file cannot find it — M1/M2/M3 below cannot run"
else

# ---------------------------------------------------------------------------
# M1 — no manifest may DECLARE a dependency its own crate never names.
#
# A `cfg(not(target_family = "wasm"))` table is part of the DESKTOP graph, so an
# addition there is an addition to the desktop build and to Cargo.lock. The only
# additions Phase 3a is entitled to make are the ones §5.5 (web-time) and §4.1
# (the getrandom dummy + the wasm-bindgen bindings its wasm socket needs) call
# for; those are listed by name and everything else has to be used by the crate.
# src/tests/ is excluded because a dev-dependency legitimately lives only there.
# ---------------------------------------------------------------------------
unused_added=$(python3 - "${repo_dir}" "${phase3a_base}" <<'MPY'
import pathlib, re, subprocess, sys, tomllib

repo, base = pathlib.Path(sys.argv[1]), sys.argv[2]

# web-time: plan §5.5, the Instant swap Phase 3b spends it on.
# getrandom/getrandom_02/web-sys/wasm-bindgen*/js-sys: plan §4.1, the wasm-only
# RPC transport, verified there as the fix for the getrandom 0.2/0.3 wall.
ALLOWED = {"web-time", "getrandom", "getrandom_02", "web-sys",
           "wasm-bindgen", "wasm-bindgen-futures", "js-sys"}


def dependency_names(text):
    manifest = tomllib.loads(text)
    names = set(manifest.get("dependencies", {}))
    for table in manifest.get("target", {}).values():
        names |= set(table.get("dependencies", {}))
    return names


def git(*arguments):
    return subprocess.run(["git", "-C", str(repo), *arguments],
                          capture_output=True, text=True).stdout


problems = []
for relative in git("diff", "--name-only", base, "--", "*Cargo.toml").split():
    if relative == "Cargo.toml":
        continue
    crate = pathlib.Path(relative).parent
    added = dependency_names((repo / relative).read_text()) \
        - dependency_names(git("show", f"{base}:{relative}"))
    for dependency in sorted(added - ALLOWED):
        identifier = re.compile(rf"\b{re.escape(dependency.replace('-', '_'))}\b")
        sources = [path for path in (repo / crate / "src").rglob("*.rs")
                   if "/tests/" not in str(path) and not path.name.endswith("_tests.rs")]
        if not any(identifier.search(path.read_text(errors="replace")) for path in sources):
            problems.append(f"{crate.name}:{dependency}")
print(",".join(sorted(set(problems))) if problems else "none")
MPY
)
expect_equal "M1 every dependency Phase 3a added to a crate manifest is used by that crate (or is a named §4.1/§5.5 addition)" \
    "none" "${unused_added}"

# ---------------------------------------------------------------------------
# M2 — a dependency the root [workspace.dependencies] already pins must be
# inherited, not re-declared. zed-web can write literal versions because it
# builds from the root workspace with its own [patch] tables; we build from a
# second workspace, so a literal here is simply a second pin that stops tracking
# the first. Only declarations Phase 3a WROTE are judged — a line that merely
# moved between tables keeps its original spec and is not flagged.
# ---------------------------------------------------------------------------
unpinned_redeclarations=$(python3 - "${repo_dir}" "${phase3a_base}" <<'MPY'
import json, pathlib, subprocess, sys, tomllib

repo, base = pathlib.Path(sys.argv[1]), sys.argv[2]


def dependency_specs(text):
    manifest = tomllib.loads(text)
    tables = [manifest.get("dependencies", {})]
    tables += [table.get("dependencies", {})
               for table in manifest.get("target", {}).values()]
    specs = {}
    for table in tables:
        for name, spec in table.items():
            specs.setdefault(name, []).append(json.dumps(spec, sort_keys=True))
    return specs


def git(*arguments):
    return subprocess.run(["git", "-C", str(repo), *arguments],
                          capture_output=True, text=True).stdout


workspace_pinned = set(tomllib.load(open(repo / "Cargo.toml", "rb"))
                       .get("workspace", {}).get("dependencies", {}))

problems = []
for relative in git("diff", "--name-only", base, "--", "*Cargo.toml").split():
    if relative == "Cargo.toml":
        continue
    before = dependency_specs(git("show", f"{base}:{relative}"))
    for name, specs in dependency_specs((repo / relative).read_text()).items():
        if name not in workspace_pinned:
            continue
        for spec in specs:
            if spec in before.get(name, []):
                continue
            parsed = json.loads(spec)
            if isinstance(parsed, dict) and parsed.get("workspace") is True:
                continue
            problems.append(f"{pathlib.Path(relative).parent.name}:{name}={spec}")
print(",".join(sorted(set(problems))) if problems else "none")
MPY
)
expect_equal "M2 every dependency Phase 3a wrote that the workspace already pins is inherited with workspace = true" \
    "none" "${unpinned_redeclarations}"

# ---------------------------------------------------------------------------
# M3 — every wasm clause Phase 3a wrote into a cfg gate must be load-bearing.
#
# Round 2 caught the inverse of this in url_wasm: a gate that fires on targets it
# was never meant to. The manifest version of the same mistake is a gate that
# fires on exactly the same set with or without its wasm clause — dead text on a
# line that decides which TLS backend the DESKTOP build links. Measured by
# evaluating the predicate, and the predicate with the wasm clause structurally
# removed, against rustc's own cfg for the host and the four wasm targets.
# ---------------------------------------------------------------------------
dead_wasm_clauses=$(python3 - "${repo_dir}" "${phase3a_base}" <<'MPY'
import pathlib, re, subprocess, sys, tomllib

repo, base = pathlib.Path(sys.argv[1]), sys.argv[2]
TARGETS = [None, "wasm32-unknown-unknown", "wasm32-wasip1", "wasm32-wasip2",
           "wasm32-unknown-emscripten"]


def cfg_of(target):
    command = ["rustc", "--print", "cfg"] + (["--target", target] if target else [])
    values = set()
    for line in subprocess.run(command, capture_output=True, text=True).stdout.split():
        if "=" in line:
            key, value = line.split("=", 1)
            values.add((key, value.strip('"')))
        else:
            values.add((line, None))
    return values


def parse(predicate):
    tokens = re.findall(r'[A-Za-z_][A-Za-z0-9_]*|"[^"]*"|[(),=]', predicate)
    position = 0

    def node():
        nonlocal position
        head = tokens[position]
        position += 1
        if head in ("all", "any", "not"):
            position += 1  # "("
            children = []
            while tokens[position] != ")":
                children.append(node())
                if tokens[position] == ",":
                    position += 1
            position += 1
            return (head, children)
        if position < len(tokens) and tokens[position] == "=":
            position += 1
            value = tokens[position].strip('"')
            position += 1
            return ("kv", head, value)
        return ("key", head)

    tree = node()
    assert position == len(tokens), f"trailing tokens in {predicate!r}"
    return tree


def evaluate(tree, values):
    kind = tree[0]
    if kind == "all":
        return all(evaluate(child, values) for child in tree[1])
    if kind == "any":
        return any(evaluate(child, values) for child in tree[1])
    if kind == "not":
        return not evaluate(tree[1][0], values)
    if kind == "kv":
        return (tree[1], tree[2]) in values
    return any(entry[0] == tree[1] for entry in values)


WASM_LEAF = ("kv", "target_family", "wasm")


def is_wasm_clause(node):
    return node == WASM_LEAF or node == ("not", [WASM_LEAF])


def without_wasm_clause(tree):
    """The predicate with its wasm clause removed; None means 'always true'."""
    if is_wasm_clause(tree):
        return None
    if tree[0] == "all":
        remaining = [child for child in tree[1] if not is_wasm_clause(child)]
        if len(remaining) == len(tree[1]):
            raise ValueError("wasm clause is nested deeper than one level")
        if not remaining:
            return None
        return remaining[0] if len(remaining) == 1 else ("all", remaining)
    raise ValueError("unrecognised shape")


def target_cfg_keys(text):
    return set(tomllib.loads(text).get("target", {}))


def git(*arguments):
    return subprocess.run(["git", "-C", str(repo), *arguments],
                          capture_output=True, text=True).stdout


cfgs = {target: cfg_of(target) for target in TARGETS}
problems = []
for relative in git("diff", "--name-only", base, "--", "*Cargo.toml").split():
    before = target_cfg_keys(git("show", f"{base}:{relative}"))
    for key in sorted(target_cfg_keys((repo / relative).read_text()) - before):
        if "target_family" not in key or '"wasm"' not in key:
            continue
        predicate = key[len("cfg("):-1] if key.startswith("cfg(") else key
        crate = pathlib.Path(relative).parent.name or "<root>"
        try:
            stripped = without_wasm_clause(parse(predicate))
        except (AssertionError, ValueError) as error:
            problems.append(f"{crate}:UNANALYSABLE[{predicate}]({error})")
            continue
        with_clause = [evaluate(parse(predicate), cfgs[t]) for t in TARGETS]
        without = [True if stripped is None else evaluate(stripped, cfgs[t])
                   for t in TARGETS]
        if with_clause == without:
            problems.append(f"{crate}:DEAD[{predicate}]")
print("; ".join(problems) if problems else "none")
MPY
)
expect_equal "M3 every wasm clause Phase 3a wrote into a cfg gate changes the gate on at least one target" \
    "none" "${dead_wasm_clauses}"

fi

# ---------------------------------------------------------------------------
# S1 — the §5.5 Instant port must not leave rustfmt work behind.
#
# `cargo fmt --all -- --check` is a CI gate (.github/actions/check_style/action.yml).
# Splitting a `use std::{..., time::Instant}` block and adding `use web_time::Instant;`
# puts the new import in the wrong slot of rustfmt's sort order, which is invisible to
# the compiler. The scope is every file that names web_time — that is exactly the port's
# own footprint, and it deliberately excludes files dirty for reasons predating this work.
#
# rustfmt is fed on stdin, not by path: given a path it descends into the file's `mod`
# children and would report another file's formatting as this one's.
# ---------------------------------------------------------------------------
unformatted_web_time_files=$(
    cd "${repo_dir}" || exit 1
    while IFS= read -r relative; do
        if [[ -n "$(rustfmt --check --edition 2024 <"${relative}" 2>/dev/null)" ]]; then
            printf '%s\n' "${relative}"
        fi
    done < <(grep -rl 'web_time' crates --include='*.rs' | sort)
)
if [[ -z "${unformatted_web_time_files}" ]]; then
    pass "S1 every .rs file naming web_time is rustfmt-clean (cargo fmt --all -- --check)"
else
    fail "S1 every .rs file naming web_time is rustfmt-clean (cargo fmt --all -- --check)" \
        "expected: 0 unformatted files" \
        "actual:   $(printf '%s\n' "${unformatted_web_time_files}" | wc -l | tr -d ' ') unformatted files:"
    printf '%s\n' "${unformatted_web_time_files}" | sed 's/^/         /'
fi

# ---------------------------------------------------------------------------
printf '\n%d checks, %d failures\n' "${checks}" "${failures}"
printf 'not covered here: §9 bullet 4, "the desktop bundle must still build and install"\n'
printf '                  (script/bundle-mac; too slow and too destructive for a gate)\n'
[[ "${failures}" -eq 0 ]]
