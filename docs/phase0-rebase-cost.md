# Phase 0 — rebase cost of overlaying `zed-web` onto `andy/web-version`

Measurement only. No files in the main worktree were changed except this report.

- **Date:** 2026-09-15
- **Main worktree:** `andy/web-version` at `ee080f343354ad3a367e35bbd95132dea535c806`
- **Theirs:** `zedweb/zed-web`
- **Merge-base (confirmed unique):** `fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8`

## Commands actually run

`git version 2.50.1 (Apple Git-155)` supports `--write-tree` and `--name-only`.

Primary (complete conflict set, no working-tree writes):

```
git merge-tree --write-tree --name-only \
  --merge-base=fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8 \
  andy/web-version zedweb/zed-web
```

Cross-check without `--merge-base` (git finds the same single merge-base):

```
git merge-tree --write-tree --name-only andy/web-version zedweb/zed-web
```

Both produced the same resulting tree `fab47523c071f3e21449ff63a1cad4795fab89ab` and the same 32 conflicted paths. Exit status 1 (conflicts present). Hunk counts below are `^<<<<<<<` in `git show fab47523:<path>`.

A throwaway `git rebase --onto` was **not** run. `merge-tree` already emitted the full set; a rebase would stop at the first conflict and could not add information.

## 1. Complete conflict set

**32 files, 70 conflict hunks.**

None of the 32 is an add/add: every path exists at the merge-base. 28 of 32 are files that `origin/main` also touched after `fecc3273`. Only 4 of 32 are in our local `origin/main..HEAD` work (and two of those are lock/manifest).

| Hunks | File | Kind | Merge strategy |
| ---: | --- | --- | --- |
| 13 | `Cargo.lock` | adjacent (web-time / which 6.0.3 / windows pin vs our lock) | regenerate after taking our `which` 8 and refusing zed-web's 6.0.3 pin; do not take theirs as a blob |
| 1 | `Cargo.toml` | adjacent (`jsonschema` 0.51 ours vs 0.37 theirs) | keep ours (0.37 is a rollback) |
| 3 | `crates/agent/src/agent.rs` | adjacent (theirs `ref_count` / `session.save_worker`; ours a new leak test) | keep both: take their fields, keep our test |
| 1 | `crates/agent_ui/src/agent_panel.rs` | adjacent (import rename) | union the two import lists |
| 1 | `crates/assets/src/assets.rs` | **structural** (ours `util::fs_embed!` vs theirs `RustEmbed` + wasm `Cow` map) | keep `fs_embed!` on native; add a wasm cfg branch beside it, do not revert to `RustEmbed` |
| 1 | `crates/editor/src/items.rs` | adjacent (ours language-detection local; theirs wasm metadata prefetch) | keep both statements |
| 1 | `crates/extensions_ui/src/extension_suggest.rs` | adjacent (import block) | union imports; keep our `ExtensionStore` / markdown / LSP uses |
| 4 | `crates/fs/src/fs.rs` | mixed: adjacent wasm `cfg` vs our `git_clone_progress`; overlapping `FakeFs` test-support | take their `not(wasm)` gates; keep `git_clone_progress`; merge FakeFs helpers by hand |
| 1 | `crates/fs/src/fs_watcher.rs` | adjacent (`std::time::Instant` vs `web_time` / `OnceLock`) | Instant → `web_time`; keep our `Pin` import |
| 1 | `crates/git_ui/src/git_graph.rs` | adjacent (timestamp formatting rewrite) | keep ours (`format_timestamp`); theirs is an older formatter |
| 1 | `crates/gpui/src/executor.rs` | adjacent (wasm `block_on_ready` vs our `not(wasm)` gate) | take their wasm helper **and** keep our native cfg |
| 1 | `crates/gpui/src/gestures.rs` | adjacent (Instant import) | `web_time::Instant`; keep `VecDeque` / `mem` |
| 1 | `crates/gpui/src/platform_scheduler.rs` | **structural-ish** (theirs wraps park-loop in wasm busy-poll vs native park) | take their `cfg(wasm)` / `cfg(not(wasm))` split and paste our native park-loop into the native arm |
| 2 | `crates/gpui/src/window.rs` | **structural** (ours `LongPress` / `TouchDrag` / `last_input_was_touch` vs theirs `show_soft_keyboard` + different Touch handling) | keep our touch/long-press model; add `show_soft_keyboard` as a new method |
| 8 | `crates/gpui_web/src/events.rs` | **structural** (ours `TouchIds` + pointer-capture/IME vs theirs `TouchPointerState` / momentum scrolling) | do not take theirs as a blob; port wasm-needed listeners onto our touch model |
| 1 | `crates/gpui_web/src/ime_mirror.rs` | adjacent (rename `element_selection_end` vs `selection_end`) | keep ours; grep-rename on the zed-web call site if we take their window |
| 3 | `crates/gpui_web/src/platform.rs` | adjacent (ours `GestureTuning`/`ActivityGuard` vs theirs clipboard/menus fields) | keep both field sets |
| 4 | `crates/gpui_web/src/window.rs` | **structural** (ours IME `TextInputConfiguration` vs theirs `show_soft_keyboard` / momentum types) | keep our IME API; add soft-keyboard methods |
| 1 | `crates/project/src/lsp_store/log_store.rs` | adjacent (Instant import) | `web_time::Instant`; keep `Weak` |
| 1 | `crates/recent_projects/src/recent_projects.rs` | adjacent (dev-container `observe_new` ours vs `OpenDevContainer` action theirs) | keep both handlers; **do not** take `files: false` (see §4 — that change auto-merged outside this hunk) |
| 2 | `crates/recent_projects/src/remote_connections.rs` | adjacent (theirs `not(wasm)` ExtensionStore register; ours empty at those lines) | take their cfg-gated block |
| 1 | `crates/remote/src/transport/docker.rs` | adjacent (ours `std::time::Instant` import; theirs moved it) | Instant → `web_time`; keep `fmt::Write` |
| 2 | `crates/rpc/src/message_stream.rs` | mixed: Instant import; zstd `cfg(wasm)` vs our decode path | Instant → `web_time`; take wasm zstd skip **without** dropping our `decode_from_slice` if that is the current API |
| 1 | `crates/rpc/src/peer.rs` | **structural** (theirs 119-line rewrite of incoming-response dispatch vs our 2-line `register_connection`) | keep `register_connection`; port any wasm-needed response-channel behaviour onto it, do not revert the helper |
| 1 | `crates/settings_json/Cargo.toml` | adjacent (ours optional tree-sitter `editing` deps; theirs omitted) | keep ours |
| 6 | `crates/settings_json/src/settings_json.rs` | adjacent (`feature = "editing"` vs `not(wasm)` on the same items) | `#[cfg(all(feature = "editing", not(target_family = "wasm")))]` |
| 1 | `crates/settings_ui/src/components/ollama_model_picker.rs` | adjacent (ours always-local list vs theirs wasm stub + native fetch) | take their cfg split, put our list in the wasm arm |
| 1 | `crates/sidebar/src/sidebar.rs` | adjacent (ours `repo_identity_path_if_local` vs theirs `not(wasm)` gate) | keep the import; cfg-gate only the wasm-broken call |
| 1 | `crates/tabular_data_preview/src/parser.rs` | adjacent (Instant import) | Instant → `web_time` |
| 1 | `crates/terminal/src/terminal.rs` | adjacent (ours `has_active_pty_resources` / `release_pty_resources` vs theirs `not(wasm)` on the next item) | keep the methods; put `#[cfg(not(wasm))]` on native-only PTY code. **Shift+Click deletion is not in this hunk** (see §4) |
| 2 | `crates/util/src/shell.rs` | adjacent (import order / blank line) | either side; keep `PathBuf` |
| 1 | `crates/util/src/util.rs` | adjacent (ours `extern crate self as util` + `not(wasm)` vs theirs empty) | keep ours; add wasm cfg beside it |

**Auto-merged (no conflict), of the files §5.4 called out:** `crates/project/src/project.rs`, `crates/remote/src/remote_client.rs`, `crates/remote/Cargo.toml`, `crates/remote_server/src/server.rs`, `crates/settings/src/vscode_import.rs`, `crates/zed/src/main.rs`, `README.md`, `.gitignore`.

## 2. §5.4 collision table — audit

The 12-file intersection is **correct** when measured as `origin/main..andy/web-version` ∩ `fecc3273..zedweb/zed-web`:

```
.gitignore
Cargo.lock
Cargo.toml
README.md
crates/project/src/project.rs
crates/recent_projects/src/recent_projects.rs
crates/recent_projects/src/remote_connections.rs
crates/remote/Cargo.toml
crates/remote/src/remote_client.rs
crates/remote_server/src/server.rs
crates/settings/src/vscode_import.rs
crates/zed/src/main.rs
```

Four trivial + eight real is the right split. Per-file shortstat against `origin/main` (ours) and `fecc3273` (zed-web) **matches the plan table exactly**:

| File | zed-web | ours vs `origin/main` |
| --- | --- | --- |
| `recent_projects.rs` | +165 −80 | +1 |
| `project.rs` | +34 −12 | +5 |
| `remote_client.rs` | +2 −1 | +464 −5 |
| `remote_connections.rs` | +2 | +20 −1 |
| `vscode_import.rs` | +2 | +1 |
| `remote/Cargo.toml` | +1 | +1 |
| `zed/src/main.rs` | +1 −1 | +4 |
| `remote_server/src/server.rs` | +1 −1 | +14 −3 |

What the table gets wrong is the **frame**, not the arithmetic:

- The plan says "29 commits, 69 files". Today `origin/main..HEAD` is **30 commits, 70 files**. The extra commit is `ee080f3433` (`docs/web-zed-plan.md`), which does not intersect zed-web. The 29/69 figure was true before that commit.
- "There is no structural conflict" is true **of those 12 files**. `merge-tree` of the two branches is a different question: 32 content conflicts, 28 of them from `origin/main` drifting past `fecc3273` (221 commits, 702 files on our side vs that merge-base), including structural `gpui_web` / `gpui::Window` / `rpc::peer` edits. §5.4 never counted that.
- Phase 3 in the plan says "the five collision files in §5.4 merged by hand" and "the three refusals in §5.3". §5.4 lists eight real files; §5.3 lists four refusals. Of the five that are not the three "will not conflict" rows, `merge-tree` actually conflicts on **two** (`recent_projects.rs`, `remote_connections.rs`). `vscode_import.rs`, `remote/Cargo.toml`, and `zed/src/main.rs` auto-merged.

### The three "will not conflict" claims

All three are **true** as merge-tree results.

1. **`remote_client.rs`.** zed-web's only change vs `fecc3273` is the import block: drop `Instant` from `std::time` (around line 48) and add `use web_time::Instant;` at line 54. Our +464 is tests starting at line 1388 (`an_external_agents_updated_push_before_handlers_is_not_answered_as_unhandled`). Auto-merged. On our HEAD the file still says `std::time::{Duration, Instant}` at line 48 — the web Instant swap still has to be applied by hand in Phase 3 even though it did not conflict.
2. **`remote_server/src/server.rs`.** zed-web's hunk is `send_blocking` → `try_send` at **line 255** (plan said 252; the `@@` header is 245, the call is 255). Ours vs `origin/main` starts at line 620 (`shell_environment_ready`). Distance 365 lines, not ~370, same conclusion. Auto-merged — which is why the refused `try_send` **lands silently** (see §4).
3. **`project.rs`.** zed-web: `#[cfg]` `terminals` / `terminals_wasm` at lines 23–27 and 144–146, plus `watch_global_configs`, `read_dir_with_types`, and `stop_all_language_servers`. Ours vs `origin/main` is five lines at ~1700 (`flush_queued_early_messages`). Auto-merged.

## 3. §5.3 refuse list — exact locations on zed-web

All four changes exist. **Three of the four apply with no conflict** in the `fab47523` merge result. Filtering them is not "skip a conflicted hunk"; it is "revert an auto-merge".

### `crates/zed/RELEASE_CHANNEL` — `dev` → `stable`

- **zed-web line 1:** `stable` with **no trailing newline** (`git show` bytes: `b'stable'`).
- Base and ours: `dev\n`.
- Diff is the whole file: `@@ -1 +1 @@`.
- **Not in the conflict list.** Merged blob is `stable`. Must restore `dev\n` after any merge.

### `crates/terminal/src/terminal.rs` — Shift+Click selection extension deleted

On zed-web, `mouse_down` for `SelectionType::Simple` + Shift (lines 2791–2794):

```
if selection_type == Some(SelectionType::Simple) && e.modifiers.shift {
    self.events
        .push_back(InternalEvent::UpdateSelection(position));
    return;
}
```

Deleted relative to base (~2615): the `if self.last_content.selection.is_some() { UpdateSelection } else { SetSelection(...) }` branch (the `#25143` extend-existing path). Our HEAD still has that branch at **2692–2704**, plus `test_terminal_shift_click_extends_existing_selection` at 4063.

The one `terminal.rs` conflict is unrelated (`has_active_pty_resources` / `release_pty_resources` vs a `not(wasm)` cfg). **The Shift+Click deletion auto-merged.** Merged blob at 2868–2871 is the zed-web `UpdateSelection`-only path. Must restore the `last_content.selection` branch.

### `crates/recent_projects/src/recent_projects.rs` — `PathPromptOptions.files` `true` → `false`

Only this site. zed-web `open_local_project`, lines **2137–2138**:

```
PathPromptOptions {
    files: false,
```

Base / ours: `open_local_project` at **2089–2090**, `files: true`.

The other `PathPromptOptions` (Open WSL folder) stays `files: true` on zed-web at lines **303–304**. That is not the refused change.

The file **does** conflict, but the hunk is the dev-container `observe_new` vs `OpenDevContainer` action (~line 514 in the merged blob). **`files: false` auto-merged** at merged line 2164. Keep `files: true` there.

### `crates/remote_server/src/server.rs` — log flush `send_blocking` → `try_send`

zed-web `MultiWrite::flush`, **line 255**:

```
.try_send(self.buffer.clone())
```

Base and ours: same line **255**, `.send_blocking(self.buffer.clone())`.

**Not in the conflict list.** Merged blob is `try_send`. Restore `send_blocking`.

## 4. One-sentence conclusion

**Phase 3 "mechanical" does not stand:** overlaying `zed-web` on this branch is 32 content conflicts / 70 hunks (not five hand-merges), several of them structural (`gpui_web` touch model, `rpc::peer`, `assets`, `gpui::Window`), and three of the four §5.3 refusals land silently because they do not conflict.

The local-feature collision in §5.4 is real and small — that table's line counts are right, and its three "no conflict" claims are right. The rebase cost that Phase 0 was asked to measure is the other number: **32 files**, driven by our tree being 221 commits past the zed-web merge-base, not by the 30 local commits past `origin/main`.
