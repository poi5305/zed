# Phase 1 review — round 1

Adversarial review of the Phase 1 workspace skeleton (`web/Cargo.toml`,
`web/.cargo/config.toml`, root `exclude = ["web"]`) against `docs/web-zed-plan.md`
§3.2, §3.3 and §9. Clean context, reviewer is not the author.

Round 1 of the review loop: findings → RED regression test → fix to green, all by one
agent, in that order.

## 1. Findings (frozen)

Output before a single line was changed. New problems met while fixing are listed
separately under `discoveredWhileFixing`.

```json
[
  {
    "id": "F1",
    "severity": "high",
    "file": "web/Cargo.toml",
    "line": 7,
    "claim": "The web workspace repeats 1 of the 5 root [patch.crates-io] entries that the wasm32 dependency graph actually reaches. async-task, async-process, notify and notify-types are patched at the root and absent here; patches do not cross workspaces (plan §3.2), so the web build resolves them to unpatched crates.io versions.",
    "why_it_matters": "async-task is reachable from gpui, scheduler AND rpc under a wasm32-unknown-unknown platform filter — the three crates least able to avoid it. Zed patches it to smol-rs/async-task rev b4486cd7. The failure is silent: the unpatched crate resolves, compiles, and behaves differently. This is exactly the 'vendored fork silently rolls a dependency backwards' risk §11 rates High, arriving through the opposite door.",
    "how_to_prove": "cargo metadata --format-version 1 --filter-platform wasm32-unknown-unknown from the repo root, walk resolve.nodes from gpui/scheduler/rpc/project/editor/remote, intersect with the root [patch.crates-io] key set. Result: {async-task} from gpui, scheduler, rpc; {async-process, async-task, notify, notify-types, tree-sitter-language} from project, editor, remote."
  },
  {
    "id": "F2",
    "severity": "high",
    "file": "web/Cargo.toml",
    "line": 1,
    "claim": "[profile.web-release] is missing. Cargo honours [profile.*] only at a workspace root, so moving the wasm build out of the root workspace (§3.2's whole point) silently drops the profile the reference build uses.",
    "why_it_matters": "zedweb/zed-web:web/build.sh:57 builds with --profile web-release, and zedweb/zed-web:Cargo.toml:1099 defines it in the ROOT manifest — a definition §3.2's layout has no home for. Our root Cargo.toml does not define it either. The first real build from web/ dies with 'profile `web-release` is not defined', and the tempting fix (add it to the root manifest) would put a wasm-only profile into the desktop workspace. §3.2 lists two things the web workspace must re-declare (workspace.dependencies keys, patch entries) and omits the third.",
    "how_to_prove": "git show zedweb/zed-web:Cargo.toml | sed -n '/^\\[profile.web-release/,/^\\[/p' shows six keys; grep -c 'profile.web-release' web/Cargo.toml and the root Cargo.toml both return 0."
  },
  {
    "id": "F3",
    "severity": "high",
    "file": "web/.cargo/config.toml",
    "line": 1,
    "claim": "web/.cargo/config.toml is discovered from the current working directory, not from the workspace root. Running the wasm build as `cargo build --manifest-path web/Cargo.toml --target wasm32-unknown-unknown` from the repo root reads NONE of these rustflags, and applies the desktop root's [build] rustflags instead.",
    "why_it_matters": "Measured, not inferred. From web/: the 13 wasm flags are on the rustc command line. From the repo root with --manifest-path: the only flags present are `-C symbol-mangling-version=v0 --cfg tokio_unstable`. No error, no warning — a wasm artifact built without +atomics, --shared-memory, --import-memory or getrandom_backend=\"wasm_js\". This is the same first-match-wins scar §3.2 cites for .cargo/bundle-config.toml, reappearing as a cwd problem rather than a precedence problem, and nothing in the skeleton detects it.",
    "how_to_prove": "cargo build -v --target wasm32-unknown-unknown from web/ vs from the repo root with --manifest-path web/Cargo.toml, with one throwaway member; diff the -C/--cfg tokens on the rustc line."
  },
  {
    "id": "F4",
    "severity": "high",
    "file": "web/.cargo/config.toml",
    "line": 4,
    "claim": "`-C target-feature=+atomics` requires a standard library rebuilt with atomics, i.e. `-Z build-std=std,panic_abort` on a nightly toolchain. `web/` resolves to the root rust-toolchain.toml — stable 1.97.1 — and nightly is not installed on this machine. Nothing in the skeleton records the requirement.",
    "why_it_matters": "zedweb/zed-web:web/build.sh:53-58 runs the wasm build under `rustup run nightly cargo build ... -Z build-std=std,panic_abort`; lines 47-51 also export CC_wasm32_unknown_unknown / CFLAGS_wasm32_unknown_unknown pointing at a downloaded wasi-sdk. §3.2 describes web/.cargo/config.toml as 'wasm target rustflags only', which is now known to be an incomplete description of what the wasm build needs. The failure mode is a link error at the very end of a long build, or worse, prebuilt non-atomic std silently linked against atomic-compiled crates.",
    "how_to_prove": "cd web && rustup show active-toolchain -> '1.97.1 ... (overridden by <repo>/rust-toolchain.toml)'; rustup toolchain list shows no nightly; git show zedweb/zed-web:web/build.sh lines 44-58."
  },
  {
    "id": "F5",
    "severity": "medium",
    "file": "Cargo.toml",
    "line": 272,
    "claim": "`exclude = [\"web\"]` blocks workspace MEMBERSHIP only, not graph inclusion — and §9's stated check ('root Cargo.lock must not gain ... any path source pointing into web/') cannot be performed against Cargo.lock at all, because path packages are recorded there with no source field and no path.",
    "why_it_matters": "Proven in an isolated fixture: a root member with `forked = { path = \"../web/vendor/forked\" }`. Without exclude, forked becomes a root workspace member. WITH exclude it is not a member — but the root Cargo.lock still contains `[[package]] name = \"forked\"` with no source and no way to tell it lives under web/. A §9 check implemented as a grep for 'web/' in Cargo.lock therefore passes while contaminated. exclude is load-bearing (it does stop membership) but it is not the guarantee §9 claims.",
    "how_to_prove": "Two-case fixture under a scratch dir; cargo metadata --no-deps member list with and without exclude, plus the generated Cargo.lock in the exclude case."
  },
  {
    "id": "F6",
    "severity": "medium",
    "file": "web/.cargo/config.toml",
    "line": 1,
    "claim": "The repo root .cargo/config.toml is merged into every cargo invocation run from web/. The wasm rustflags survive only because target.* wins over build.* in first-match-wins order — a property nothing asserts.",
    "why_it_matters": "Proven: `cd web && cargo xtask --help` expands the root config's alias and fails with 'package(s) `xtask` not found in workspace <repo>/web', so the root config is demonstrably in scope. Two ways this breaks later, both silent: (a) a [target.wasm32-unknown-unknown] section added to the ROOT config would CONCATENATE with web's rather than replace it; (b) RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS in the environment replaces both — and zedweb/zed-web:web/build.sh:45 does exactly `export RUSTFLAGS=...`, so adopting that script later re-introduces the override §3.2 forbids.",
    "how_to_prove": "cd web && cargo xtask --help; then grep the discovery chain (web/.cargo/config.toml, <repo>/.cargo/config.toml, ~/.cargo/config.toml) for a [target.wasm32-unknown-unknown] section, and check RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS in the environment."
  },
  {
    "id": "F7",
    "severity": "low",
    "file": "web/Cargo.toml",
    "line": 3,
    "claim": "With members = [], `cargo metadata` (with deps) fails outright — 'manifest path ... contains no package: The manifest is virtual, and the workspace has no members' — so web/Cargo.lock, which §3.2's layout lists as a file, cannot exist yet, and the [patch.crates-io] table is never validated by the resolver. Every command run from web/ that does resolve also warns 'patch `tree-sitter-language` was not used in the crate graph'.",
    "why_it_matters": "It means no Phase 1 assertion can be made about the web lock file, and the patch warning will be permanent noise until the first member lands. Not a defect in the skeleton — it is what 'empty' means — but it caps what the regression test can check today.",
    "how_to_prove": "cd web && cargo metadata --format-version 1 (exit 101).",
    "disposition": "wontfix"
  },
  {
    "id": "F8",
    "severity": "low",
    "file": "web/Cargo.toml",
    "line": 2,
    "claim": "resolver = \"2\" while members will be edition 2024, whose own default is resolver 3 (MSRV-aware selection). Cargo emits no warning for the mismatch.",
    "why_it_matters": "Verified with an edition-2024 throwaway member: no diagnostic. The root workspace is also resolver = \"2\", so the two workspaces agree, which is worth more than matching the edition default — the shared crates/ tree is resolved by both and should see the same feature-unification rules.",
    "how_to_prove": "Throwaway edition-2024 member in web/, cargo build -v: no resolver diagnostic emitted.",
    "disposition": "wontfix"
  },
  {
    "id": "F9",
    "severity": "low",
    "file": "web/Cargo.toml",
    "line": 5,
    "claim": "The empty [workspace.dependencies] table fails the first member intelligibly, so §3.2's concern is already answered.",
    "why_it_matters": "Measured message: 'error inheriting `anyhow` from workspace root manifest's `workspace.dependencies.anyhow` / Caused by: `dependency.anyhow` was not found in `workspace.dependencies`', naming the member, the workspace manifest and the missing key. Nothing to fix.",
    "how_to_prove": "Throwaway member with `anyhow.workspace = true`, cargo metadata --no-deps from web/.",
    "disposition": "wontfix"
  },
  {
    "id": "F10",
    "severity": "low",
    "file": "Cargo.toml",
    "line": 272,
    "claim": "`exclude = [\"web\"]` also removes future web crates from `xtask package-conformity` and from script/generate-licenses, both of which iterate the ROOT workspace's packages.",
    "why_it_matters": "tooling/xtask/src/tasks/package_conformity.rs:15-19 walks load_workspace().workspace_packages(), so web members will never be checked for workspace lints or non-workspace dependencies, and their licences will never reach the generated bundle. That is an unavoidable consequence of the chosen layout, not a bug in it, but it is a gap someone should own before the web build ships.",
    "how_to_prove": "Read tooling/xtask/src/tasks/package_conformity.rs:14-26 and script/generate-licenses' workspace scope.",
    "disposition": "wontfix"
  },
  {
    "id": "F11",
    "severity": "none",
    "file": "script/bundle-mac",
    "line": 90,
    "claim": "Nothing in this diff changes desktop bundling. Checked rather than assumed.",
    "why_it_matters": "script/bundle-mac runs from the repo root, passes explicit --package zed/cli/remote_server, and the only manifest it rewrites is crates/zed/Cargo.toml (lines 97-105). web/.cargo/config.toml can never be in scope for it because config discovery starts at the cwd. .gitignore's `**/target` already covers web/target, and rust-toolchain.toml already lists wasm32-unknown-unknown. Root member count is 257 before and after, and the root Cargo.lock is unchanged by any cargo command run from web/.",
    "how_to_prove": "cargo metadata --no-deps from the root with a throwaway crate present under web/ (257 members, crate absent); git status --short Cargo.lock after the web/ probes (clean); git check-ignore -v web/target.",
    "disposition": "wontfix"
  }
]
```

Counts: 11 findings — 4 high (F1–F4), 2 medium (F5, F6), 4 low (F7–F10), 1 none (F11).
Fixed: F1, F2, F3, F4, F5, F6. Marked `wontfix`: F7, F8, F9, F10, F11.

## 2. RED — the regression test before the fixes

`web/check-workspace-isolation.sh` turns §9's invariants into assertions that print
the actual value beside the expected one. macOS has no coreutils `timeout`, so every
cargo call goes through a 300-second `perl -e 'alarm'` wrapper; the only command that
produces artefacts runs under `CARGO_TARGET_DIR=target/web-probe`, never the shared
desktop `target/`.

Two of §9's four bullets could not be written the way §9 words them:

- **"Root `Cargo.lock` must not gain … any `path` source pointing into `web/`"** is
  unimplementable against `Cargo.lock` (F5). A path package is recorded there with no
  `source` field and no path, so a grep for `web/` passes while contaminated. The check
  asks `cargo metadata` for every root package's `manifest_path` instead.
- **"The desktop bundle must still build and install"** is out of reach for a gate —
  `script/bundle-mac` is far too slow and rotates the shared `target/release` the plan
  forbids touching. The script prints that it does not cover it rather than pretending.

First run, against the skeleton exactly as submitted:

```
ok   §9.1 the nine crates resolve to their recorded sources in the root Cargo.lock
ok   §9.2 no root-workspace package has a manifest under web/
ok   §9.2 root workspace member count is unchanged
ok   §9.2 root workspace_root
ok   §3.2 web workspace_root is web/, not the repo root
ok   §9 web build does not share target/ with the desktop build
ok   §3.2 wasm rustflags equal zedweb/zed-web:web/build.sh:45
ok   F6 no config above web/ redefines [target.wasm32-unknown-unknown]
ok   F6 RUSTFLAGS is not set (it would replace the config rustflags)
ok   F6 CARGO_ENCODED_RUSTFLAGS is not set (it would replace the config rustflags)
FAIL F1 every wasm-reachable root [patch.crates-io] entry is repeated in web/Cargo.toml
       expected: none
       actual:   async-process,async-task,notify,notify-types
FAIL F2 web/Cargo.toml declares [profile.web-release] as zedweb/zed-web:Cargo.toml:1099 does
       expected: codegen-units=1 debug=False inherits=release lto=thin opt-level=z strip=symbols
       actual:   <[profile.web-release] not declared>
FAIL F4 the -Z build-std / nightly requirement of +atomics is either satisfied or recorded
       expected: a nightly toolchain for web/, or a 'build-std' note in web/.cargo/config.toml
       actual:   toolchain '1.97.1-aarch64-apple-darwin (overridden by '/Users/andy/go/src/github.com/poi5305/zed/rust-toolchain.toml')' and no build-std note in web/.cargo/config.toml
       see:      zedweb/zed-web:web/build.sh:53-58
FAIL F3 web/.cargo/config.toml records that it is discovered from the working directory
       expected: a note explaining that cargo --manifest-path web/Cargo.toml run from the
                 repo root reads none of these flags and applies the desktop ones instead
       actual:   no such note

14 checks, 4 failures
```

### 2.1 The ten that were green from the start

Ten assertions passed on the unfixed skeleton, which proves nothing on its own. Each
was broken deliberately, observed red, and restored — file checksums compared before
and after.

| Break | Assertion | Observed |
| --- | --- | --- |
| `which 8.0.5` → `8.0.6` in the root `Cargo.lock` | §9.1 | `FAIL` with a line diff naming both versions |
| `RUSTFLAGS="-C opt-level=0"` in the environment | F6 | `FAIL … expected: <unset> / actual: -C opt-level=0` |
| `[target.wasm32-unknown-unknown]` appended to the **root** `.cargo/config.toml` | F6 | `FAIL … actual: <repo>/.cargo/config.toml` |
| `-C link-arg=--shared-memory` deleted from `web/.cargo/config.toml` | §3.2 rustflags | `FAIL` printing both 13- and 12-flag strings |
| `exclude = ["web"]` removed **and** `web/probe-crate` added to root `members` | §9.2 ×2 | `FAIL … actual: <repo>/web/probe-crate/Cargo.toml` and `expected: 257 / actual: 258` |

`Cargo.lock`, `Cargo.toml`, `.cargo/config.toml` and `web/.cargo/config.toml` all
returned to their original MD5s afterwards. The remaining green assertions
(`workspace_root`, `target_directory`, `CARGO_ENCODED_RUSTFLAGS`) share their code
paths with a broken-and-restored sibling.

## 3. Fixes

Four findings fixed in code, two fixed by making the check itself correct, five
`wontfix`. Every `src/`-equivalent change below maps to exactly one finding; nothing
else in the tree was touched.

| Finding | Change | File |
| --- | --- | --- |
| F1 | Added the four wasm-reachable root patch entries — `async-process`, `async-task`, `notify`, `notify-types` — verbatim from the root manifest, with a comment recording that the list is the measured intersection rather than a guess | `web/Cargo.toml` |
| F2 | Added `[profile.web-release]`, byte-identical to `zedweb/zed-web:Cargo.toml:1099`, with a comment on why it cannot be inherited | `web/Cargo.toml` |
| F3 | Recorded that the file is discovered from the working directory, what the repo-root invocation silently substitutes, and that every wasm build must run with `web/` as cwd | `web/.cargo/config.toml` |
| F4 | Recorded the nightly + `-Z build-std=std,panic_abort` requirement and the wasi-sdk `CC_`/`CFLAGS_` exports, pointing at `zedweb/zed-web:web/build.sh:53` | `web/.cargo/config.toml` |
| F5 | The `web/` check is done through `cargo metadata` manifest paths, not through `Cargo.lock` | `web/check-workspace-isolation.sh` |
| F6 | Asserts the config discovery chain and the two environment overrides | `web/check-workspace-isolation.sh` |

Reconciliation: 11 findings = 6 fixed (F1–F6) + 5 `wontfix` (F7–F11). 14 assertions,
4 of them red before the fixes (F1, F2, F3, F4) and 10 proven red by deliberate
breakage. The two medium findings produced no code change because the defect was in
the *check*, not in the skeleton — which is why they appear as assertions rather than
as edits.

### 3.1 F3 and F4 are documentation ratchets, not functional guards

Said plainly, because the distinction matters for round 2.

**F3 cannot be fixed by configuration.** Cargo resolves `.cargo/config.toml` from the
working directory; there is no key that makes a config file bind to a workspace. The
flags cannot be moved to the root config either — `target.*` entries from different
config files *concatenate*, so the desktop workspace would start carrying wasm link
arguments, and a repo-root wasm build would then be subtly different from a `web/` one
instead of obviously wrong. Reporting it as unfixable is the accurate answer; the
comment plus the assertion is the most the skeleton can carry.

**F4's real fix belongs to `web/build.sh`**, which arrives with Phase 4. Pinning
`web/rust-toolchain.toml` to nightly would put the whole web workspace — including
rust-analyzer and every `cargo check` — on nightly to serve one build step, which is
not what the reference does. `nightly` is not installed on this machine at all, so no
assertion about a working wasm link can be written today. The check is therefore
conditional: **either** `web/` resolves to a nightly toolchain, **or** the gap is
written down. It will flip to the first branch by itself once a nightly override exists.

### 3.2 Spec gaps for the brain, not defects

- **§3.2 lists two things the web workspace must re-declare and needs a third.**
  `[workspace.dependencies]` keys and `[patch.crates-io]` entries are named; `[profile.*]`
  is not, and it is the one that fails at the first real build (F2). The same paragraph
  should say which patch entries, and on what evidence — the wasm32-filtered reachability
  computation now lives in `web/check-workspace-isolation.sh` and should be cited there.
- **§3.2 describes `web/.cargo/config.toml` as "wasm target rustflags only".**
  `zedweb/zed-web:web/build.sh:47-58` shows that a working wasm build additionally needs
  a nightly toolchain, `-Z build-std=std,panic_abort`, and `CC_wasm32_unknown_unknown` /
  `CFLAGS_wasm32_unknown_unknown` pointed at a downloaded wasi-sdk. The description is
  not wrong about this file; it is misleading about what Phase 1 has finished.
- **§9's second bullet asks for a check that cannot be performed as worded** (F5).
  Suggested rewording: "no package in the root workspace's `cargo metadata` may have a
  `manifest_path` under `web/`" — `Cargo.lock` cannot express it.
- **§3.2's `exclude = ["web"]` claim is right but for a narrower reason than stated.**
  Measured: `exclude` does stop a path dependency from a root member being auto-adopted,
  but the package still appears in the root `Cargo.lock`. And the walk-up case §3.2
  worries about (`cd web && cargo build` joining the root workspace) is already prevented
  by `web/Cargo.toml`'s own `[workspace]` table, which stops the walk before `exclude` is
  ever consulted — the error a stray crate gets names `web/Cargo.toml`, not the root.
- **F10** — web members will be invisible to `xtask package-conformity` and to
  `script/generate-licenses`, both of which iterate the root workspace's packages. That
  is inherent to the layout and needs an owner before the web build ships.

`discoveredWhileFixing`: none. Two bugs were found in the first draft of the check
itself (a heredoc shadowing a piped stdin, and `CARGO_TARGET_DIR=` set to the empty
string rather than unset); both were in the test, not in the reviewed diff, so they are
not findings.
