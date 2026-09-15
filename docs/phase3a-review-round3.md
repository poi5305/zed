# Phase 3a review — round 3

Date: 2026-09-15. Branch `andy/web-version`. Clean context, adversarial, not the author.
Scope: every `Cargo.toml` in `git diff` (49 crates + root) and the resulting `Cargo.lock`.
Out of scope and untouched: all `.rs` (Phase 3b is editing them concurrently), `web/Cargo.toml`,
`web/vendor/`, `web/check-refusals.sh`, existing `docs/` reports. No commit, no `git stash` /
`checkout` / `restore` / `reset`.

Baseline snapshot: `cp Cargo.lock /tmp/lock-before-review3` before any edit.

CodeGraph was called first (`codegraph_explore "debugger_ui tree_sitter tree_sitter_json usage in
crates/debugger_ui"`) and, as in Phase 2 and Phase 3a, returned Rust symbols unrelated to the
question (`claude_sessions::usage`, `language_model_core::chat_completion`, `editor::inlays`) — the
index does not model Cargo manifests, and the query's crate scope was ignored. Cross-checked with
grep, which gave the answer directly. Reported per the "CodeGraph 結果可疑時交叉驗證並回報差異" rule:
for manifest-shaped questions the index is not usable, and this is now the third phase to hit it.

---

## 1. Findings (frozen before any edit)

```json
[
  {
    "id": "R3-1",
    "severity": "high",
    "file": "crates/debugger_ui/Cargo.toml",
    "line": "77-79",
    "claim": "Phase 3a ADDED two dependencies our tree never had (tree-sitter-json, tree-sitter with features=[\"wasm\"]) under [target.'cfg(not(target_family = \"wasm\"))'.dependencies]. Because the gate is not(wasm), they land in the DESKTOP dependency graph, and Cargo.lock grew two entries under debugger_ui to match. crates/debugger_ui has zero non-test references to tree_sitter / tree_sitter_json; the only tree-sitter use in the crate is src/tests/inline_values.rs, served by the pre-existing dev-dependency tree-sitter-go (Cargo.toml:89).",
    "why_it_matters": "Violates minimal diff and grows the desktop dependency graph and lockfile with crates nothing compiles against. The mistake is a pattern, not a typo: zed-web's debugger_ui sources DO use them, so its manifest gates them; ours does not, so the correct port is not to touch debugger_ui at all. The author's own report states this knowingly (\"our tree never had these in [dependencies] ... added them under not(wasm) as zed-web did\").",
    "how_to_prove": "git show HEAD:crates/debugger_ui/Cargo.toml | grep tree-sitter -> only tree-sitter-go (dev-dependencies). grep -rn 'tree_sitter' crates/debugger_ui/src -> hits only under src/tests/. git diff -U0 -- Cargo.lock | grep -E '\"tree-sitter' -> two added lines."
  },
  {
    "id": "R3-2",
    "severity": "medium",
    "file": "crates/rpc/Cargo.toml",
    "line": "46-48",
    "claim": "wasm-bindgen = \"0.2\", wasm-bindgen-futures = \"0.4\" and js-sys = \"0.3\" are declared as literal version requirements even though the root [workspace.dependencies] already pins all three (Cargo.toml:908, 909, 677 -- wasm-bindgen at 0.2.120). Copied verbatim from zed-web, which puts its patch tables in the root workspace and therefore has different constraints.",
    "why_it_matters": "A second, looser declaration site for a version the workspace already pins. Today both resolve to the locked 0.2.120 / 0.4.70 / 0.3.97 so nothing moves, but a future bump of the workspace pin silently stops applying to crates/rpc, and the two sites can diverge with no file in the diff changing. Every other first-party manifest in the repo inherits from the workspace table.",
    "how_to_prove": "grep -nE '^(js-sys|wasm-bindgen|wasm-bindgen-futures) *=' Cargo.toml shows workspace entries exist; crates/rpc/Cargo.toml:46-48 does not use them. web-sys and getrandom/getrandom_02 have no workspace entry, so they are correctly literal."
  },
  {
    "id": "R3-3",
    "severity": "low",
    "file": "crates/client/Cargo.toml",
    "line": "68",
    "claim": "The wasm-side rpc dependency is written rpc = { path = \"../rpc\", default-features = false, features = [\"gpui\"] } -- a raw path dependency bypassing the workspace table (Cargo.toml has rpc = { path = \"crates/rpc\" }), and default-features = false is inert because crates/rpc declares no `default` feature (only `gpui` and `test-support`).",
    "why_it_matters": "Same class as R3-2: a second declaration site for a first-party crate, plus a knob that reads as load-bearing and is not. The native arm one line above (65) uses the workspace form, so the file contradicts itself.",
    "how_to_prove": "sed -n '/^\\[features\\]/,/^\\[dependencies\\]/p' crates/rpc/Cargo.toml -> no `default` key. grep -n '^rpc = ' Cargo.toml -> workspace entry exists."
  },
  {
    "id": "R3-4",
    "severity": "low",
    "file": "crates/client/Cargo.toml",
    "line": "87",
    "claim": "[target.'cfg(all(not(target_family = \"wasm\"), any(target_os = \"windows\", target_os = \"macos\")))'.dependencies] -- the not(target_family = \"wasm\") clause is dead. No wasm target reports target_os windows or macos, so the predicate evaluates identically with and without it on wasm32-unknown-unknown, wasm32-wasip1, wasm32-wasip2, wasm32-unknown-emscripten and the host. The sibling table at line 90 (the not(any(...)) arm) genuinely needs it.",
    "why_it_matters": "An edit to a cfg that gates a desktop-only TLS backend, for zero effect. Minimal diff: a reviewer of a later sync has to re-derive that it does nothing, and the asymmetry with line 90 invites 'fixing' the wrong one.",
    "how_to_prove": "rustc --print cfg --target wasm32-unknown-unknown|wasm32-wasip1|wasm32-wasip2|wasm32-unknown-emscripten: target_os is unknown/wasi/wasi/emscripten on all four, never windows or macos."
  },
  {
    "id": "R3-5",
    "severity": "low",
    "file": "Cargo.toml",
    "line": "403",
    "claim": "languages = { path = \"crates/languages\", default-features = false } is a provable no-op. crates/languages declares no `default` feature (only test-support and load-grammars), so Cargo synthesises default = [] and disabling it selects nothing. §4.1 item 3 asks for this line 'so the web binary never enables load-grammars', but load-grammars was never a default feature -- crates/zed, markdown, edit_prediction, eval_cli and edit_prediction_cli each opt in explicitly.",
    "why_it_matters": "Answers the brain's most dangerous question: measured, the desktop feature graph is unchanged apart from the empty `languages feature \"default\"` node disappearing. It is however a latent trap -- if crates/languages ever gains a real `default`, this line silently disables it for the desktop build too.",
    "how_to_prove": "cargo tree -p zed -e features --target aarch64-apple-darwin, with and without the clause: the ONLY difference is two lines, `languages feature \"default\"` and `languages feature \"default\" (*)`. No dependency, no other feature. Full diff pasted below."
  },
  {
    "id": "R3-6",
    "severity": "low",
    "file": "crates/settings_ui/Cargo.toml",
    "line": "30",
    "claim": "cpal.workspace = true was left in plain [dependencies] while its transitive sibling `audio` was moved to not(wasm). cpal is the native audio backend and its only consumers in this crate (src/pages/audio_input_output_setup.rs:2, src/pages/audio_test_window.rs:2) sit alongside the audio code that was gated.",
    "why_it_matters": "Inconsistent gating: the wasm graph still names cpal. Desktop is unaffected (a not(wasm) move never removes anything from desktop), so this is a Phase 3b/4 build failure waiting rather than a regression.",
    "how_to_prove": "grep -rn cpal crates/settings_ui/src; compare with the audio move at crates/settings_ui/Cargo.toml:72-77."
  },
  {
    "id": "R3-7",
    "severity": "low",
    "file": "crates/keymap_editor/Cargo.toml",
    "line": "35",
    "claim": "tempfile.workspace = true left in plain [dependencies], although the author's own stated rationale for moving it in crates/agent (\"tempfile pulls errno, unsupported on wasm32-unknown-unknown\") and crates/agent_servers and crates/fs applies identically here; keymap_editor uses tempfile::TempDir at src/keymap_editor.rs:463, 583, 3274, 3407, 3415.",
    "why_it_matters": "Same class as R3-6: the errno edge §4.1 calls the wall is only partly pruned. Desktop unaffected.",
    "how_to_prove": "grep -rn tempfile crates/keymap_editor/src; compare with crates/agent/Cargo.toml:87-90."
  },
  {
    "id": "R3-8",
    "severity": "low",
    "file": "crates/*/Cargo.toml",
    "line": "various (e.g. client:19, rpc:21, fs:21, project:37)",
    "claim": "web-time.workspace = true was inserted as the FIRST line of [dependencies] in roughly 30 of the 38 manifests that gained it, ahead of anyhow, breaking the alphabetical ordering every one of those blocks otherwise follows. A handful (agent:77, git_ui:74, terminal:47) got it in a third, different position.",
    "why_it_matters": "Pure diff noise, but it is the tell that the insertion was mechanical and unchecked, and it makes the next upstream sync of these blocks conflict more than it needs to.",
    "how_to_prove": "git diff -- 'crates/*/Cargo.toml' | grep -B2 '^+web-time'"
  }
]
```

### What was checked and came back clean (so the findings above are the whole list)

- **cfg spelling.** Every one of the 20 target tables Phase 3a wrote uses `target_family = "wasm"`,
  the §4.1 form. Not one bare `target_arch = "wasm32"` — the round-2 `url_wasm` mistake did not
  recur. R3-4 is the *opposite* error (a correct-but-dead clause), not that one.
- **Direction of every move.** All 37 moved dependencies went to the `not(wasm)` side. Only three
  things sit on the wasm side (`client`'s rpc, `sidebar`'s `agent_ui`, `rpc`'s wasm bindings), and
  no native-only crate was moved there. A `not(wasm)` move is desktop-inert by construction, so no
  move can regress the desktop build.
- **Net-new dependencies.** Computed per manifest by parsing `HEAD:` and the worktree with
  `tomllib` and diffing the name sets, rather than reading `+` lines. Result: `web-time` × 38
  (§5.5, expected, Phase 3b spends it), the six wasm-only crates in `crates/rpc` (§4.1), and
  R3-1's two. Nothing else. Dev- and build-dependencies: zero changes in all 49 files.
- **§5.3.** No manifest route exists for any of the four refusals: `crates/zed/Cargo.toml`,
  `crates/zed/RELEASE_CHANNEL` and `crates/remote_server/Cargo.toml` are all unmodified, and
  `web/check-refusals.sh` is 4/4 green before and after this round.
- **`Cargo.lock`.** Zero deletions, zero `[[package]]` blocks added or removed, zero
  `version =` / `source =` / `checksum =` lines touched. Verified independently of the author's
  report.
- **Manifest validity.** `cargo metadata` over the whole workspace exits 0 with an *empty* stderr —
  no cargo warning about an unused manifest key, a `dep:` reference into a target-gated optional
  dependency (`crates/settings_json`'s `editing` feature), or a duplicated dependency key.

### R3-5 measurement — the desktop `languages` feature set

The brain flagged this as the most dangerous item because `default-features = false` on a workspace
dependency is global, not wasm-scoped. Measured rather than argued, by capturing the whole desktop
feature graph twice:

```
$ cargo tree -p zed -e features --target aarch64-apple-darwin --prefix none | sort -u   # with the clause
$ <remove ", default-features = false"; re-run>                                          # without
$ diff with without
2205a2206,2207
> languages feature "default"
> languages feature "default" (*)
```

5,571 lines each; two lines of difference, both the empty synthesised `default` feature, and
`Cargo.lock` byte-identical across the toggle. No dependency and no other feature changes on the
desktop target. The root manifest was restored from a `cp` backup, not by a git command.

---

## 2. RED

Three assertions appended to `web/check-workspace-isolation.sh`. The existing 28 were not
modified, reordered, or deleted. All three compare against the pinned commit
`ee080f343354ad3a367e35bbd95132dea535c806` rather than `HEAD`, so they keep their meaning once
Phase 3a is committed instead of silently becoming vacuous.

- **M1** — no manifest may declare a dependency its own crate never names, outside the allowlist
  §4.1/§5.5 names (`web-time`, `getrandom`, `getrandom_02`, `web-sys`, `wasm-bindgen`,
  `wasm-bindgen-futures`, `js-sys`). This is R3-1's pattern turned mechanical. `src/tests/` is
  excluded, because a dependency used only there belongs in `[dev-dependencies]`, which M1 never
  reads.
- **M2** — a dependency the root `[workspace.dependencies]` already pins must be inherited with
  `workspace = true`. Judged only on declarations Phase 3a *wrote*: a line that merely moved between
  tables keeps its spec and is not flagged.
- **M3** — every wasm clause Phase 3a wrote into a cfg gate must be load-bearing. The predicate and
  the predicate with the wasm clause structurally removed are both evaluated against `rustc --print
  cfg` for the host and for `wasm32-unknown-unknown`, `wasm32-wasip1`, `wasm32-wasip2`,
  `wasm32-unknown-emscripten`. Equal on all five means dead. A clause nested deeper than one level
  is reported `UNANALYSABLE` rather than silently passed.

```
ok   §9.1 the nine crates resolve to their recorded sources in the root Cargo.lock
... 27 more pre-existing assertions, all ok ...
ok   V11 cd web && cargo check --workspace --target wasm32-unknown-unknown succeeds
FAIL M1 every dependency Phase 3a added to a crate manifest is used by that crate (or is a named §4.1/§5.5 addition)
       expected: none
       actual:   debugger_ui:tree-sitter,debugger_ui:tree-sitter-json
FAIL M2 every dependency Phase 3a wrote that the workspace already pins is inherited with workspace = true
       expected: none
       actual:   client:rpc={"default-features": false, "features": ["gpui"], "path": "../rpc"},rpc:js-sys="0.3",rpc:wasm-bindgen-futures="0.4",rpc:wasm-bindgen="0.2"
FAIL M3 every wasm clause Phase 3a wrote into a cfg gate changes the gate on at least one target
       expected: none
       actual:   client:DEAD[all(not(target_family = "wasm"), any(target_os = "windows", target_os = "macos"))]

31 checks, 3 failures
exit=1
```

Each failure prints the expected value and the actual offending crate, dependency and spec — not
"it threw". No assertion can hang (the three are pure `tomllib` + `rustc --print cfg`), so none
needed the script's `run_with_timeout` wrapper; this machine has no `timeout` binary and the script
already uses `perl -e alarm` where a timeout is required.

---

## 3. Fixes

| Finding | Fix |
| --- | --- |
| R3-1 | `crates/debugger_ui/Cargo.toml`: the whole added `[target.'cfg(not(target_family = "wasm"))'.dependencies]` table deleted. The file is now **byte-identical to HEAD** — the correct port of this manifest was to leave it alone. The manifest diff drops from 50 files to 49. |
| R3-2 | `crates/rpc/Cargo.toml:46-48`: `wasm-bindgen` / `wasm-bindgen-futures` / `js-sys` now `.workspace = true`. `web-sys`, `getrandom` and `getrandom_02` keep their literal specs — no workspace entry exists for them, and §4.1 quotes the `getrandom_02` line verbatim. |
| R3-3 | `crates/client/Cargo.toml`: because `crates/rpc` has no `default` feature, the wasm arm resolved identically to the native arm, so the split carried no information. `rpc = { workspace = true, features = ["gpui"] }` restored to plain `[dependencies]` at its original position and the wasm-only table removed. This line is now unchanged from HEAD. |
| R3-4 | `crates/client/Cargo.toml:87`: reverted to `cfg(any(target_os = "windows", target_os = "macos"))`. The sibling at line 90 keeps its `not(target_family = "wasm")`, which M3 confirms is load-bearing (on wasm32-unknown-unknown the bare `not(any(windows, macos))` is *true* and would pull `tokio-rustls`). |
| R3-5 | `wontfix`. Measured inert on desktop, and §4.1 item 3 asks for the line explicitly; the author's report already records that it is currently a no-op. Removing it would contradict the spec for no gain. Flagged as a latent trap for whoever gives `crates/languages` a real `default`. |
| R3-6 | `wontfix`. Desktop-inert. Gating `cpal` correctly is a Phase 3b/4 call that needs the `.rs` half, which this round may not touch. |
| R3-7 | `wontfix`. Same reasoning as R3-6. |
| R3-8 | `wontfix`. Cosmetic; re-sorting ~30 blocks would add more churn than it removes. |

Reconciliation: **8 findings → 4 fixed, 4 `wontfix`; 3 RED assertions (M1 ⊃ R3-1, M2 ⊃ R3-2+R3-3,
M3 ⊃ R3-4); 4 `src/`-side edits, one per fixed finding.** Every hunk in `git diff` for
`crates/debugger_ui`, `crates/rpc` and `crates/client` maps to exactly one finding above.
`discoveredWhileFixing`: none.

### `Cargo.lock`

```
$ diff /tmp/lock-before-review3 Cargo.lock
4943d4942
<  "tree-sitter",
4945d4943
<  "tree-sitter-json",
```

Exactly R3-1's two lines removed, nothing else. Against `HEAD` the lock is now **44 insertions, 0
deletions**: 38 × `"web-time"` plus the six wasm-only edges under `crates/rpc`
(`getrandom 0.2.16`, `getrandom 0.3.4`, `js-sys`, `wasm-bindgen`, `wasm-bindgen-futures`,
`web-sys`). No `[[package]]` block, version, source or checksum changed — the R3-2 fix is
lock-neutral because the workspace pins (`wasm-bindgen 0.2.120`, `wasm-bindgen-futures 0.4.70`,
`js-sys 0.3.97`) are what the literal requirements already resolved to.

---

## 4. Green

```
31 checks, 0 failures        web/check-workspace-isolation.sh   (28 pre-existing + 3 new)
 4 checks, 0 failures        web/check-refusals.sh
```

### Narrow diff-only validation of the three new boundaries

Each new guard was asked the required question — *what legitimate input could this kill?* — and the
answer was built and measured. Probes were applied to a real manifest, run, and reverted; the tree
is byte-identical afterwards (all three report `none`).

| Probe | Guard | Result |
| --- | --- | --- |
| add `diagnostics.workspace = true` to `git_ui` (a dependency `git_ui/src` genuinely names) | M1 | `none` — a real, used, non-allowlisted addition is not killed |
| add `heed.workspace = true` to `git_ui` (unused) | M1 | `git_ui:heed` — still catches the real thing |
| add `diagnostics = { workspace = true, features = [] }` | M2 | `none` — workspace form with extra features is not killed |
| move `tokio-native-tls = "0.3"` between target tables, spec unchanged | M2 | `none` — a pure table move is not killed |
| add `diagnostics = { path = "../diagnostics" }` | M2 | `git_ui:diagnostics={"path": "../diagnostics"}` — still catches the real thing |
| `cfg(all(not(target_family = "wasm"), unix))` | M3 | `none` — correct: `wasm32-unknown-emscripten` *is* `unix`, so the clause is load-bearing there |
| `cfg(all(not(target_family = "wasm"), target_os = "linux"))` | M3 | `project:DEAD[...]` — still catches a genuinely dead clause |

M3's one acknowledged limitation: a wasm clause nested more than one level deep inside `any(...)`
is reported `UNANALYSABLE` rather than evaluated. That fails loudly and asks for a human, which is
the correct failure direction; nothing in the current tree hits it.

---

## 5. Not covered by this round

- **The desktop build was not compiled.** Phase 3b is editing `.rs` in 58 crates concurrently, so a
  `cargo check` failure could not be attributed to this diff. The desktop-graph claims here rest on
  `cargo metadata` (exit 0, no warnings), `cargo tree -e features` and the lock diff instead. The
  desktop build must be re-checked once 3b settles.
- **R3-6 / R3-7** are real wasm-side gaps left open on purpose; they will surface as the first
  `cargo check --target wasm32-unknown-unknown` failures for `settings_ui` and `keymap_editor` in
  Phase 4, and should be fixed there together with their `.rs`.
- **Spec gap, for the brain to rule on, not a defect:** §4.1 item 3 prescribes
  `languages = { default-features = false }` on the premise that it stops `load-grammars`. In our
  tree `load-grammars` is not a default feature, so the line cannot do that. Either the spec should
  record that the line is inert for us and kept only for parity with `zed-web`, or a later phase
  should give `crates/languages` the `default` feature §4.1 assumes. Left as written, and the
  desktop feature set is measured unchanged either way.
