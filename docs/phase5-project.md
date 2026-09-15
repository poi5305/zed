# Phase 5 — wasm `project` + `prompt_store`

Date: 2026-09-15. Branch `andy/web-version`, HEAD `f35188dd8b`. **No commit.** `web/` source was not edited. `README.md` `> [!IMPORTANT]` lines left in place. The five pre-dirty rustfmt files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`) were not touched.

Specs: `docs/web-zed-plan.md` Phase 5「Rulings on behaviour that could not be preserved」and the `tree_sitter::Language` BLOCKER. cfg spelling: `target_family = "wasm"` only. **No `unsafe impl Send` / `unsafe impl Sync`.**

This is a continuation of HEAD, not a rewrite. Only `crates/prompt_store/src/prompt_store.rs` was edited this round. The ten files already committed at HEAD were left as they compiled.

Baseline lock: `cp Cargo.lock /tmp/lock-p2` before the honesty edit. Lock unchanged (`cmp` identical, 503887 bytes).

## 0. codegraph (required first call)

```
codegraph explore "PromptStore new load save delete heed PromptDb spawn_project_work Pin Box Future Send language"
```

Returned 19 symbols across 12 files. Second call:

```
codegraph explore "PromptStore save delete save_metadata first_user_prompt next_title_for_prompt_id bodies env metadata_cache spawn_project_work"
```

`PromptStore` has no `save` / `delete` in this tree (HEAD~1 already did not). Blast radius of `load` / `new` stays in `prompt_store.rs` plus `rules_to_skills_migration.rs`. Manifests are not in the graph; confirmed by grep / `git show`:

- `crates/prompt_store/Cargo.toml` already gates `heed` under `not(target_family = "wasm")` (HEAD, matching zed-web).
- `crates/http_client` already stubs `github_download` on wasm with `Err("…not available in the browser")`. The `agent_server_store.rs` `http_client::github` errors from `docs/phase5-worktree.md` are gone without a further `project` edit.

## 1. What HEAD already did (not redone)

`git diff HEAD~1 -- crates/project crates/prompt_store` (10 files, +145 / −33):

| Site | Native | Wasm |
| --- | --- | --- |
| `spawn_project_work!` in `project.rs` | `$cx.background_spawn($future)` | `$cx.foreground_executor().spawn($future)` — same shape as `crates/language/src/buffer.rs` for `!Send` `Language` |
| `LocalLspAdapterDelegate` / `DapAdapterDelegate` | `#[async_trait]` | `#[async_trait(?Send)]` |
| `lsp_store.rs` language-server start | `background_spawn` | `foreground_executor().spawn` |
| LSP resolve / execute / prettier reload / vue / document colors / links | `background_spawn` | `spawn_project_work!` |
| `project_settings.rs` user tasks / debug scenarios | `block_on` first item, then chain | skip `block_on`; same watcher stream (SettingsStore ruling) |
| `terminals.rs` `exec_in_shell` | `smol::process::Command::from` | `Err("exec_in_shell cannot spawn a local process in the browser")` |
| `WorktreeUpdatedEntries` telemetry | `report_discovered_project_type_events` | cfg'd out (method absent on wasm) |
| `prompt_store` | heed LMDB `Env` | in-memory cache + builtins; `heed` import / `upgrade_dbs` / V1 types `not(wasm)` |

`language.rs` in the same HEAD commit dropped `Send + Sync` from `LspAdapterDelegate` on wasm (`lsp_adapter_delegate_bounds`). That is the BLOCKER leaking into `dyn LspAdapterDelegate: !Send`, which is why `languages` later fails `impl Send + Future`. Not undone. No `unsafe impl`.

## 2. What this round added

zed-web's `prompt_store` (`git diff fecc3273ed..zedweb/zed-web -- crates/prompt_store`) is **not** `sql_rpc`. `sql_rpc` is sqlite (`AppDatabase`). `heed`/LMDB has no RPC in zed-web either; they used an in-memory `HashMap` plus builtins. HEAD copied that.

That is a fabricated success for **user-saved** rows: `load(User { … })` returned `"prompt not found"`, which looks like the library is empty rather than unavailable. The Phase 5 rule is loud failure (`panic!` naming the alternative, or `Err`), never a silent empty.

Kept HEAD's structure (builtins stay; `new()` still `Ok`; native body untouched). Changed only the user-id miss:

- `PromptStore::new` (wasm): `log::warn!` that heed/LMDB is not available; builtins are loaded, user-saved prompts are not.
- `PromptStore::load` (wasm, user id, no in-memory body): `anyhow::bail!("prompt library persistence (heed/LMDB) is not available in the browser")` instead of `"prompt not found"`.
- Built-in ids still return `BuiltInPrompt::default_content()` (compiled-in, not LMDB).

`new()` was **not** switched to `Err`. That would drop builtins from `PromptStore::global()` and undo HEAD + zed-web. AppDatabase's `panic!` does not apply: there is no in-browser alternative named for LMDB, and builtins do not need mmap.

## 3. `prompt_store` on web — actual limit

| Capability | Native | Web |
| --- | --- | --- |
| Open LMDB at `prompts-library-db.0.mdb` | yes (`heed::EnvOpenOptions`, 1 GB map) | no; mmap / host fs |
| Built-in prompt metadata + body | yes (DB row or `default_content()`) | yes (`with_builtins()` + `default_content()`) |
| User-saved prompts from disk | yes | **not loaded**. `all_prompt_metadata()` lists builtins only. `load(User)` is `Err` naming heed/LMDB |
| V1 → V2 upgrade | `upgrade_dbs` | cfg'd out (no env) |
| Persist new user prompts | this crate has no `save`/`delete` on any target | same |

There is no heed RPC and none will appear without a new protocol. sql_rpc does not substitute.

## 4. Native unchanged

Every wasm `cfg` has a `not(target_family = "wasm")` arm with the pre-existing statements:

- `spawn_project_work!` native = `background_spawn`.
- `project_settings` still `block_on`s the first config file.
- `terminals` still `smol::process::Command::from`.
- telemetry `report_discovered_project_type_events` still runs.
- `PromptStore` still has `env: heed::Env`, `upgrade_dbs`, and LMDB `load`. The honesty `bail!` is inside `#[cfg(target_family = "wasm")]`.

Native check (root workspace, stable, `CARGO_BUILD_JOBS=4`):

```
CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=target/web-probe cargo check -p project -p prompt_store --lib
```

```
    Checking prompt_store v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/prompt_store)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.27s
```

Exit 0. `project` was already cached from HEAD's native-clean compile; this round did not edit it.

## 5. Wasm check (acceptance 1)

cwd `web/` so `web/.cargo/config.toml` rustflags apply. Nightly + `-Z build-std=std,panic_abort` because `[target.wasm32-unknown-unknown] rustflags` includes `+atomics` (same as `docs/phase5-worktree.md`). `CARGO_BUILD_JOBS=4`. One cargo at a time.

```
cd web && CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=../target/web-probe rustup run nightly cargo check -p project -p prompt_store --target wasm32-unknown-unknown --lib -Z build-std=std,panic_abort
```

```
    Checking prompt_store v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/prompt_store)
warning: `prompt_store` (lib) generated 1 warning (1 duplicate)
    Checking project v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/project)
warning: `project` (lib) generated 1 warning (1 duplicate)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 24s
```

No `error[` in `crates/project` or `crates/prompt_store`. The duplicate warning is the workspace-wide inferred-readme lint.

## 6. Other gates

```
./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking
4 checks, 0 failures
```

`rustfmt --edition 2024 --check` on `prompt_store.rs` and the nine HEAD `project` files → all clean.

Root `Cargo.lock` vs `/tmp/lock-p2`: identical, 503887 bytes. `git diff --stat -- Cargo.lock` vs HEAD is empty.

## 7. `./web/build.sh` — `project` / `prompt_store` gone; next wall is `acp_thread`

`CARGO_BUILD_JOBS=4`, `RUSTFLAGS` unset. Native server half: `Finished release … in 1.05s` (cached). Exit 101. ~114s.

Wasm compiled `prompt_store` and `project` (`warning: project (lib) generated 1 warning` after the failed crate). **No** `error[` under those two crates. **No** `could not compile \`project\`` / `prompt_store`.

This run's only compile failure:

```
error: could not compile `acp_thread` (lib) due to 2 previous errors
```

| Crate | `error[` | What |
| --- | ---: | --- |
| `acp_thread` | 2 | `portable_pty` gated in `Cargo.toml` (`not(wasm)`); `acp_thread/src/terminal.rs:504` and `:542` still name `portable_pty::ExitStatus`. Same manifest/.rs seam as Phase 5 layer 2. |

`languages` did not start under `JOBS=4` before `acp_thread` failed. It is still the BLOCKER leak. A prior `web/build.sh` on the same HEAD (before the honesty edit) recorded it in `/tmp/web-build-proj.txt`:

| Crate | `error[` / `error:` | What |
| --- | ---: | --- |
| `languages` | 18 | `AvailableGrammar: !Send` on `impl LspAdapter for JsonLspAdapter`; `impl Send + Future` capturing `Arc<dyn LspAdapterDelegate>` (`bash.rs`, `c.rs`, …); `smol::fs::DirEntry` missing `file_type`/`is_dir`/`is_file`; `RustLspAdapter::GITHUB_ASSET_KIND` / `ARCH_SERVER_NAME` |
| `acp_thread` | 2 | same `portable_pty` as this run |

Those `languages` errors are the Phase 5 BLOCKER (`Language` stays `!Send`) plus leftover native-only APIs. They are **not** `project` / `prompt_store`. Fixing `LspInstaller`'s `impl Send + Future` bound lives in `crates/language`, not here.

## 8. Files

- `crates/prompt_store/src/prompt_store.rs` — wasm `log::warn!` on `new`; user `load` `bail!` names heed/LMDB
- `docs/phase5-project.md` — this report

Not edited: `web/**` (source), `Cargo.lock`, root `Cargo.toml`, any `crates/project/**` file, the five pre-dirty rustfmt files.
