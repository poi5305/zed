# Phase 3b — review round 4

Date: 2026-09-15. Branch `andy/web-version`. Reviewer: clean-context Opus, adversarial, not the author.
Scope: every `.rs` change in `git diff` (72 files, 43 crates). `Cargo.toml` / `Cargo.lock` / `web/` out of scope.
Specs: `docs/web-zed-plan.md` §5.1, §5.2, §5.3, §5.5, Phase 3. Author report: `docs/phase3b-wasm-cfg.md`.

Order is fixed and was followed: findings JSON → RED output → fix. Nothing was edited before §1 was written.

## 1. Findings (frozen)

```json
[
  {
    "id": "R4-1",
    "severity": "medium",
    "file": "38 of the 72 changed .rs files (full list in §1.1)",
    "line": "import blocks",
    "claim": "All 72 files were rustfmt-clean at HEAD. 38 of them are rustfmt-dirty after 3b. The port inserted `use web_time::Instant;` and `#[cfg(...)] use std::time::Instant;` at the point where the old `use std::{..., time::Instant}` sat, which is the wrong slot in rustfmt's sort order, and in crates/rpc/src/peer.rs it shortened a generic argument so the enclosing type now fits on one line. `cargo fmt --all -- --check` is a CI gate (.github/workflows/run_tests.yml:166 and .github/actions/check_style/action.yml:9).",
    "why_it_matters": "3b's own stated acceptance was minimum diff. Instead every file it touched now carries formatting noise that the next `cargo fmt --all` will rewrite, which will bury the port's real hunks in an unrelated reformat commit and makes `git blame` on the Instant swap useless. The style job is already red on this branch for unrelated pre-existing reasons (crates/claude_sessions/src/claude_sessions_panel.rs is dirty at HEAD and 3b never touched it), so this does not newly break CI - it deepens an existing hole by 38 files.",
    "how_to_prove": "For each changed file: `git show HEAD:$f | rustfmt --check --edition 2024` produces no output for all 72; `rustfmt --check --edition 2024 < $f` produces a diff for 38. Feeding via stdin is required - passing a path makes rustfmt descend into `mod` children and conflates other files."
  },
  {
    "id": "R4-2",
    "severity": "low",
    "file": "crates/util/src/archive.rs",
    "line": "68 and 146",
    "claim": "`#[cfg(unix)] pub async fn extract_zip` (68) and the new `#[cfg(target_family = \"wasm\")] pub async fn extract_zip` (146) are both true on wasm32-unknown-emscripten, which reports target_family=\"unix\" AND target_family=\"wasm\". That is E0428, two definitions of the same name. This is the round-2 `url_wasm` shape again: a wasm selector wider than wasm32-unknown-unknown.",
    "why_it_matters": "A cfg pair that is disjoint on every target the project builds, but not disjoint in general, is a trap for whoever adds the next target. The same widening is what round 2 had to undo in url_wasm and round 3 had to undo in crates/client/Cargo.toml.",
    "how_to_prove": "`rustc --print cfg --target wasm32-unknown-emscripten` prints both `target_family=\"unix\"` and `target_family=\"wasm\"` plus bare `unix`. wasm32-wasip1/p2 print only `target_family=\"wasm\"`, so they are unaffected."
  },
  {
    "id": "R4-3",
    "severity": "low",
    "file": "crates/util/src/shell_env.rs",
    "line": "36-43",
    "claim": "Same overlap as R4-2 in `capture`: on emscripten `#[cfg(unix)] return capture_unix(...)` and the new `#[cfg(target_family = \"wasm\")] { ... }` tail block are both live, so the tail block is unreachable. It still type-checks; the cost is an `unreachable_code` warning, not an error.",
    "why_it_matters": "Same class as R4-2. Listed separately so the count of overlapping gates is on the record rather than folded into one line.",
    "how_to_prove": "Same `rustc --print cfg` output as R4-2; `capture_unix` is `#[cfg(unix)]` so it exists on emscripten."
  },
  {
    "id": "R4-4",
    "severity": "info",
    "file": "crates/git_ui/src/git_runtime_diagnostics.rs",
    "line": "115-118",
    "claim": "The wasm `collect_process_tree` stub returns `Value::Object(Map::new())`, an empty object, while the native one returns `{zed_pid, descendant_count, descendants: []}`. `gather()` inserts it under `\"processes\"` either way, so on the web build a consumer that reads `processes.descendant_count` gets null rather than 0.",
    "why_it_matters": "A stub whose shape differs from the real thing pushes the difference to whoever reads the dump. Nothing reads it today, and the file is byte-identical to zedweb/zed-web here.",
    "how_to_prove": "Read the two bodies at 72-113 and 115-118; `Map` is in scope from line 24."
  },
  {
    "id": "R4-5",
    "severity": "info",
    "file": "crates/proto/src/typed_envelope.rs, crates/proto/src/macros.rs vs crates/client/src/client.rs",
    "line": "typed_envelope.rs:182, macros.rs:5, client.rs import block",
    "claim": "3b retyped `TypedEnvelope::received_at` and `build_typed_envelope(received_at:)` to `web_time::Instant`, but `crates/client/src/client.rs` - in the wasm graph, absent from web/wasm-std-instant.allowlist, deliberately deferred to Phase 4 - still imports `std::time::Instant`. On native the two are the same type so nothing changes. On wasm32-unknown-unknown this converts what used to be a runtime panic into a compile error.",
    "why_it_matters": "A compile error is strictly better than a panic, so this is not a regression - but it means `crates/client` is now load-bearing for the first wasm build attempt, which Phase 4 needs to know. §5.5's gate (web/check-wasm-time.sh) cannot run until zed_web_workspace exists, so nothing else will flag it.",
    "how_to_prove": "Inventory `std::time::Instant` across crates/ and diff against `git show zedweb/zed-web:web/wasm-std-instant.allowlist`; client.rs appears in ours and not in the allowlist."
  }
]
```

### 1.1 The 38 files behind R4-1

`acp_thread/src/acp_thread.rs`, `activity_indicator/src/activity_indicator.rs`,
`agent_ui/src/buffer_codegen.rs`, `agent_ui/src/conversation_view.rs`, `agent_ui/src/terminal_codegen.rs`,
`client/src/telemetry.rs`, `codestral/src/codestral.rs`, `context_server/src/client.rs`,
`edit_prediction_context/src/bm25_context.rs`, `extension_host/src/extension_host.rs`,
`fs/src/fs.rs`, `fs/src/fs_watcher.rs`, `git_ui/src/git_graph.rs`,
`gpui/src/platform/threaded_dispatcher.rs`, `http_proxy/src/proxy/connection.rs`,
`language/src/buffer.rs`, `language/src/syntax_map.rs`, `lsp/src/lsp.rs`,
`multi_buffer/src/transaction.rs`, `project/src/buffer_store.rs`, `project/src/git_store.rs`,
`project/src/git_store/job_debug_queue.rs`, `project/src/lsp_store.rs`,
`project_panel/src/project_panel.rs`, `proto/src/typed_envelope.rs`, `remote/src/remote_client.rs`,
`remote/src/transport/docker.rs`, `remote/src/transport/ssh.rs`, `rpc/src/message_stream.rs`,
`rpc/src/peer.rs`, `tabular_data_preview/src/parser.rs`,
`tabular_data_preview/src/renderer/preview_view.rs`, `tabular_data_preview/src/tabular_data_preview.rs`,
`terminal/src/alacritty/hyperlinks.rs`, `terminal_view/src/terminal_element.rs`, `text/src/text.rs`,
`workspace/src/toast_layer.rs`, `worktree/src/worktree.rs`.

### 1.2 What was checked and found clean

These are the questions this round was pointed at. Each was answered, and the answer was "no defect":

- **No new `#[cfg]` closes a native path.** Every gate 3b added is `target_family = "wasm"` or its negation.
  `target_family` is `"unix"` or `"windows"` on every desktop target, never `"wasm"`, so each
  `cfg(not(target_family = "wasm"))` item stays compiled and each `cfg(target_family = "wasm")` item
  stays absent. This is a property of the predicate, not of the successful compile, so it also covers
  the cfg'd-out functions that have no caller and would not have made the compiler complain.
- **cfg spelling matches the convention round 3 settled.** 3b wrote `target_family = "wasm"` in all 20
  `.rs` gates; not one bare `target_arch = "wasm32"`. Round 3 (`docs/phase3a-review-round3.md:101`) had
  already ruled that form correct for the manifests, and zedweb/zed-web spells all six of these files the
  same way. R4-2/R4-3 are the only two places where that form overlaps another gate.
- **Nothing from §5.3 arrived by the `.rs` route.** `crates/zed/RELEASE_CHANNEL` untouched;
  `recent_projects.rs` and `remote_server/src/server.rs` not in the diff at all; `terminal.rs`'s diff is
  exactly two hunks (split the `use std::{...}` block, add a comment, add `use web_time::Instant;`) and
  deletes nothing, so the Shift+Click block is intact by construction and not merely by the one grep in
  `check-refusals.sh`.
- **Nothing from §5.2 was re-applied.** `language_settings.rs`, `prettier_store.rs` and the `prettier`
  crate are not in the diff. `editor.rs` and `language/src/buffer.rs` carry Instant hunks only.
- **No clock source changed.** The diff adds no `BackgroundExecutor::now()` call. Every Instant edit is a
  spelling change (`std::time::Instant` -> `web_time::Instant`), and off wasm32-unknown-unknown `web_time`
  re-exports `std::time`, so the two names denote one type. `proto_client.rs` already took its value from
  `cx.background_executor().now()` before 3b; only its import changed.
- **The port is faithful to upstream.** Every line 3b added was diffed against the corresponding
  `fecc3273..zedweb/zed-web` hunk. Ten lines differ, all trivial: eight are the residual `time::Duration,`
  left behind by splitting an import block, one is `rpc/src/peer.rs` using the imported short name `Instant`
  where upstream wrote `web_time::Instant`, one is `preview_view.rs` doing the same. `proto_client.rs` is
  the one file with no upstream counterpart, and it is §5.5's own planted defect being fixed.
- **No signature or API break.** `proto::build_typed_envelope`, `TypedEnvelope::received_at`,
  `zlog::Timer::{start_time, warn_if_longer_than, warn_if_gt}` and `search`'s `Delegate` fields changed the
  *spelling* of their types, never the types. `dap::adapters`' re-export of `latest_github_release` is
  unchanged on native.
- **No half-converted file.** Every file that now imports `web_time` was checked for a residual
  `std::time::Instant` path. The only hits are the eleven intentional `#[cfg(not(target_family = "wasm"))]`
  dual imports, the three comments that keep the allowlist's token, and `acp_thread.rs`'s two test-only
  uses - which is exactly the count the allowlist records for that file.
- **No wasm branch references a name its crate cannot reach.** `agent_ui` has `extension.workspace = true`
  and imports `Arc`; `util` already names `collections::HashMap` on the native path; `serde_json::Map` is
  imported at `git_runtime_diagnostics.rs:24`.
- **The over-broad gate has no live victim in this repo.** Extensions under `extensions/` depend on
  `zed_extension_api` alone, whose only dependencies are `serde`, `serde_json` and `wit-bindgen`. No
  workspace crate is compiled for `wasm32-wasip1`, `wasm32-wasip2` or `wasm32-unknown-emscripten`, which is
  what holds R4-2 and R4-3 at low.

## 2. Regression test (RED)

One new assertion, **S1**, appended to `web/check-workspace-isolation.sh`. None of the 31 existing
assertions was modified or removed. It reads every `.rs` file under `crates/` that names `web_time` —
the port's own footprint, 69 files — and feeds each to `rustfmt --check` **on stdin**. Passing a path
instead would make rustfmt descend into that file's `mod` children and blame this file for another
file's formatting; on stdin it cannot resolve modules, so each file is judged alone.

Scoping to "names `web_time`" rather than "the whole repo" is deliberate and was verified non-arbitrary:
all 72 files 3b touched were rustfmt-clean at HEAD, and the five `web_time` files 3b did *not* touch
(`gpui/src/app/visual_test_context.rs`, `gpui_web/examples/hello_web/main.rs`, `gpui_web/src/dispatcher.rs`,
`scheduler/src/clock.rs`, `ui/src/components/scrollbar.rs`) are all clean too. So every failure the
assertion can report is 3b's. A repo-wide assertion would have been red for pre-existing reasons —
`crates/claude_sessions/src/claude_sessions_panel.rs` is dirty at HEAD and 3b never touched it — and would
have proved nothing about this round.

```
$ ./web/check-workspace-isolation.sh
…
ok   M3 every wasm clause Phase 3a wrote into a cfg gate changes the gate on at least one target
FAIL S1 every .rs file naming web_time is rustfmt-clean (cargo fmt --all -- --check)
       expected: 0 unformatted files
       actual:   38 unformatted files:
         crates/acp_thread/src/acp_thread.rs
         crates/activity_indicator/src/activity_indicator.rs
         crates/agent_ui/src/buffer_codegen.rs
         crates/agent_ui/src/conversation_view.rs
         crates/agent_ui/src/terminal_codegen.rs
         crates/client/src/telemetry.rs
         crates/codestral/src/codestral.rs
         crates/context_server/src/client.rs
         crates/edit_prediction_context/src/bm25_context.rs
         crates/extension_host/src/extension_host.rs
         crates/fs/src/fs.rs
         crates/fs/src/fs_watcher.rs
         crates/git_ui/src/git_graph.rs
         crates/gpui/src/platform/threaded_dispatcher.rs
         crates/http_proxy/src/proxy/connection.rs
         crates/language/src/buffer.rs
         crates/language/src/syntax_map.rs
         crates/lsp/src/lsp.rs
         crates/multi_buffer/src/transaction.rs
         crates/project/src/buffer_store.rs
         crates/project/src/git_store.rs
         crates/project/src/git_store/job_debug_queue.rs
         crates/project/src/lsp_store.rs
         crates/project_panel/src/project_panel.rs
         crates/proto/src/typed_envelope.rs
         crates/remote/src/remote_client.rs
         crates/remote/src/transport/docker.rs
         crates/remote/src/transport/ssh.rs
         crates/rpc/src/message_stream.rs
         crates/rpc/src/peer.rs
         crates/tabular_data_preview/src/parser.rs
         crates/tabular_data_preview/src/renderer/preview_view.rs
         crates/tabular_data_preview/src/tabular_data_preview.rs
         crates/terminal/src/alacritty/hyperlinks.rs
         crates/terminal_view/src/terminal_element.rs
         crates/text/src/text.rs
         crates/workspace/src/toast_layer.rs
         crates/worktree/src/worktree.rs

32 checks, 1 failures
```

Red for the right reason: 38 named files, 31/31 of the pre-existing assertions still green, and the
failure line carries the actual count and the actual list rather than "it failed". Two representative
diffs behind the count:

```
crates/project/src/git_store/job_debug_queue.rs:1:
+use std::collections::VecDeque;
 #[cfg(not(target_family = "wasm"))]
 use std::time::Instant;
-use std::collections::VecDeque;
 #[cfg(target_family = "wasm")]
 use web_time::Instant;

crates/rpc/src/peer.rs:72:
-            Option<
-                HashMap<
-                    u32,
-                    oneshot::Sender<(proto::Envelope, Instant, oneshot::Sender<()>)>,
-                >,
-            >,
+            Option<HashMap<u32, oneshot::Sender<(proto::Envelope, Instant, oneshot::Sender<()>)>>>,
```

The second one is not import ordering: shortening `web_time::Instant` to the imported `Instant` made the
enclosing type fit on one line, and rustfmt collapses it. That is why the fix has to be rustfmt rather
than hand-sorting imports.

## 3. Fix (GREEN)

R4-1 only. `rustfmt --edition 2024 --emit stdout` applied on stdin to each of the 38 files, so no file
outside that list was rewritten (the other 31 `web_time` files were checked, found clean, and skipped; the modified
`.rs` count is still exactly 72).

```
$ ./web/check-workspace-isolation.sh | tail -8
ok   M1 every dependency Phase 3a added to a crate manifest is used by that crate (or is a named §4.1/§5.5 addition)
ok   M2 every dependency Phase 3a wrote that the workspace already pins is inherited with workspace = true
ok   M3 every wasm clause Phase 3a wrote into a cfg gate changes the gate on at least one target
ok   S1 every .rs file naming web_time is rustfmt-clean (cargo fmt --all -- --check)

32 checks, 0 failures

$ ./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures

$ CARGO_TARGET_DIR=target/web-probe cargo check -p acp_thread -p activity_indicator -p agent_ui \
    -p auto_update -p client -p clock -p codestral -p context_server -p dap -p dap_adapters \
    -p edit_prediction -p edit_prediction_context -p editor -p extension -p extension_host -p fs \
    -p git_ui -p gpui -p gpui_util -p http_proxy -p keymap_editor -p language -p language_tools \
    -p lsp -p multi_buffer -p oauth_callback_server -p project -p project_panel -p proto -p remote \
    -p rpc -p search -p settings -p tabular_data_preview -p terminal -p terminal_view -p text \
    -p theme -p ui -p util -p workspace -p worktree -p zlog --lib
…
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 20.25s
```

All 43 changed crates, 0 errors. `Cargo.lock` still `1 file changed, 44 insertions(+)` — untouched by
this round. `./script/clippy` was not run; no `--release --all-features` cargo command was run.

The `.rs` diff moves from `72 files changed, 197 insertions(+), 83 deletions(-)` to
`72 files changed, 199 insertions(+), 112 deletions(-)`. The growth is entirely import blocks collapsing
back onto one line after `time::Instant` left them, plus the two `peer.rs` / `preview_view.rs` reflows
above. Audited by listing every added and removed line that is not a `use`, `#[cfg]`, comment or bracket:
the residue is exactly the Instant spellings, the eight cfg-gate bodies, and those two reflows. Nothing
else moved.

## 4. Reconciliation

| id | severity | disposition | RED test | fix |
| --- | --- | --- | --- | --- |
| R4-1 | medium | **fixed** | S1 | 38 files reformatted |
| R4-2 | low | `wontfix` | — | — |
| R4-3 | low | `wontfix` | — | — |
| R4-4 | info | `wontfix` | — | — |
| R4-5 | info | `wontfix` | — | — |

5 findings, 1 RED assertion, 1 fix, 4 `wontfix`. Every `src/` hunk added by this round maps to R4-1:
`git diff` against the pre-round state touches only the 38 files S1 named, and only their formatting.

**Why R4-2 and R4-3 are `wontfix` rather than a one-word fix.** Narrowing `target_family = "wasm"` to
`all(target_arch = "wasm32", target_os = "unknown")` would remove the emscripten overlap, but it would
also make six files differ from `zedweb/zed-web` byte-for-byte, and `web/sync-upstream.sh` will keep
re-proposing upstream's spelling on every sync. The overlap has no victim: `rustc --print cfg` shows it
requires `wasm32-unknown-emscripten`, nothing in this workspace is built for emscripten or WASI
(extensions depend on `zed_extension_api`, whose only dependencies are `serde`, `serde_json` and
`wit-bindgen`), and round 3 already ruled `target_family = "wasm"` the house form for exactly this
reason. Recorded so that adding an emscripten or WASI target becomes a deliberate decision with two known
sites to fix rather than a surprise.

**What this round did not cover.** The 174-file `WASM_CFG` bucket minus the 72 done here. `crates/client`,
`crates/project`, `crates/settings`, `sqlez`, `net/async_net.rs` and the `terminals`/`terminals_wasm` swap
are Phase 4's, and R4-5 records the one way 3b made Phase 4's job louder rather than quieter.

## Suggested .rules additions

Offered for review, not merged — per `docs/AGENTS.md`'s rules-hygiene process. One candidate, which met
the "repeatedly encountered" bar inside this round alone (38 hits):

> Splitting a `use std::{..., time::Instant}` block to add `use web_time::Instant;` leaves the new import
> in the wrong slot of rustfmt's sort order. The compiler never complains. Run
> `rustfmt --check --edition 2024 < path/to/file.rs` — on **stdin**, since passing a path makes rustfmt
> descend into the file's `mod` children and report their formatting as this file's.

