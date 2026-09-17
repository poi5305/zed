# Phase 6 — what the browser does with the four features, measured

Date: 2026-09-16. Branch `andy/web-version`, from `959371c99f`. This is the first session to
drive the browser build rather than only build it, and the first to ask the server the same
questions the browser asks.

The reported symptom was "the web version opens, but project manager, claude sessions and tmux
sessions do not work — do they need a remote connection?" The answer is no, and the reason they
look broken is not in any of the three features.

## 1. The server half is correct, and there is now a layer that says so

`docs/web-zed-handoff.md` §5 lists three verification layers — `cargo check`, the link step, and
loading it in a browser. All three answer questions about the **client**. When a panel is empty
in the browser, none of them separates "the RPC is unimplemented" from "the client never called
it" from "the client called it and dropped the answer". Every one of those wears the same
costume: an empty panel.

`web/rpc-probe.mjs` is the missing layer. It speaks the wire protocol `web/crates/wasm_rpc`
speaks — a hand-rolled WebSocket handshake over a TCP socket, because Node's global `WebSocket`
cannot send the `Cookie` header the session needs — and calls the server directly. No
dependencies and no browser.

Against a server rooted at this repository, the default sweep is green:

```
ok   Home::dirs        {"home":"/Users/andy","config":"…/.config/zed","data":"…","os":"macos"}
ok   Fs::root          "/Users/andy/go/src/github.com/poi5305/zed"
ok   Workspace::ui_state
ok   Fs::load          <config>/projects.json — the full project list
ok   Process::output   tmux list-sessions → poc, zed, zed-claude-mirror-76859-44
ok   ClaudeSessions::list_sessions → 2 live sessions

6 probes, 0 failures
```

Those six cover all three features, because each reaches the server by a different route and the
sweep exercises the real one:

| Feature | How it reaches the server on wasm |
| --- | --- |
| `project_manager` | `RemoteFs` → `Fs::load` on `<config>/projects.json`. No RPC of its own. |
| `tmux_sessions` | The panel's **local** branch. `project.remote_client()` is `None` in the browser, so `refresh` shells out — and on wasm `util::command::Command` wraps `smol_wasm`'s, which routes to `Process::output`. The probe sends the exact argv `remote::tmux_sessions` builds. |
| `claude_sessions` | `WebSource` (`claude_sessions_panel.rs:1244`) → `ClaudeSessions::*`. |

**So "does this need a remote connection?" is answered: no.** The browser's "remote" is already
the machine the server runs on; `project_manager` refuses `ssh://`, `wsl://` and `docker://`
entries for exactly that reason (§6.4), and the other two never wanted one.

## 2. What is actually broken: every panel loses its state at attach

Loaded in Chrome, the window draws — title bar, status bar, WebGPU. All nine panels attach. The
docks then render nothing, and clicking the project-panel icon in the status bar does nothing.
Hovering it says `Close Left Dock ^B`, so the dock believes it is **open**; it just has no
content.

The console says why, and it says it in a pattern that is hard to misread. Every single
`panel attached` line is immediately preceded by the same error:

```
[ERROR] workspace::dock: SQLite is not supported on wasm
zed_web_workspace: project panel attached
[ERROR] workspace::dock: SQLite is not supported on wasm
zed_web_workspace: terminal panel attached
[ERROR] workspace::dock: SQLite is not supported on wasm
zed_web_workspace: project manager panel attached
… ×9, then: zed_web_workspace: docks ready (saved workspace layout restored)
```

The "saved workspace layout restored" line is reporting a success that did not happen.

About twenty-five errors at startup, all of one family, from exactly two places:

| Message | Source | Meaning |
| --- | --- | --- |
| `synchronous SQL read_kvp is not supported on wasm; use async query! which routes through sqlez::remote_sql` | `crates/db/src/query.rs`, ten-plus sites | a call site used the **synchronous** `query!` form |
| `SQLite is not supported on wasm` | `crates/sqlez/src/connection_wasm.rs:13`, `:53`, `migrations_wasm.rs:12`, `statement_wasm.rs:42` | something opened a real `sqlez::Connection` instead of going through `sqlez::remote_sql` |

Hit: `workspace::dock` (×8), `workspace::persistence`, `workspace` (select toolchains),
`terminal_view::terminal_panel`, `git_ui::git_panel`, `outline_panel`, `agent_ui::agent_panel`,
`agent_ui::thread_metadata_store`, `agent_ui::terminal_thread_metadata_store`, `db::kvp`,
`prompt_store::rules_to_skills_migration`.

**The remote SQL path is not missing — it is wired and it works.** `init_app_state` installs
`set_sql_endpoint`, `set_sql_rpc_endpoint` and `set_async_sql_client`; the server implements
`Sql::query`, `Sql::batch`, `Sql::script`, `Sql::migrate`, `Sql::bootstrap_kvp` and `Sql::reset`.
The transport is fine. The call sites chose the synchronous door.

This is the shape §2b of the handoff names as the commonest failure, one level up: not a manifest
out of step with a `use`, but **a wasm-capable async API sitting beside the synchronous one that
everything actually calls.** Nothing tells you which door a call site went through until it
executes.

## 3. The shell environment, and the toast that names it

The status bar carries a toast: `Failed to open /Users/andy/go/src/github.com/poi5305/zed`.
It is not about opening the project — `open_paths succeeded` in the same console. It comes from
`crates/project/src/environment.rs:324`, a `with_context` closure that pushes that exact string
to the user when the stat above it fails:

```
[ERROR] project::environment: Failed to load shell environment for directory "…/zed":
        stat "…/zed": std::fs::Metadata cannot be synthesized on WASM; use RemoteFs / Fs::is_dir instead
[ERROR] project::git_store: failed to get working directory environment for repository "…/zed"
```

`load_directory_shell_environment` calls `smol::fs::metadata` (`:323`) only to ask **whether the
path is a directory**, so it can pick the path or its parent. On wasm `smol` is
`web/vendor/smol_wasm`, whose `metadata` is a deliberate refusal: `std::fs::Metadata` has no
public constructor, so it genuinely cannot be synthesised. The stub is right; the caller is
wrong. `Fs::is_dir` exists, `RemoteFs` implements it, and the server answers it.

Worth noting for its own sake: the capture itself would have worked. `util::shell_env::capture`
shells out, and shelling out on wasm routes to `Process::output` — which the probe proves runs
real commands. One `stat` is standing between the browser and a real `PATH`.

## 4. Smaller things the same session turned up

- **`Fs::load_bytes` on `<root>/.git` fails three times per load**: `not a file: …/.git`. In this
  repository `.git` is a directory; something is probing it as a gitdir pointer file. Not yet
  established whether that is a benign probe or a defect.
- **The server writes a `.config/zed/` tree under whichever directory it serves.** Serving this
  repository produced `<repo>/.config/zed/settings.json`, `global_settings.json` and `snippets/`,
  while `Home::dirs` correctly reports `/Users/andy/.config/zed`. Some path is being resolved
  relative to the process's working directory. Left untracked and **not** added to `.gitignore`,
  because ignoring it would hide the defect rather than fix it.
- **`.zed/remote.sqlite*`, `.zed/web-workspace-state.json` and `.zed/web-auth-token`** are server
  state written next to the project settings `.zed/` legitimately tracks. These *are* artifacts,
  and are now in `.gitignore`.

## 4b. The three panels were never broken — they could not be opened

Added after the fix round. The user's report was *"file explore / terminal 都可以用了 但 tmux
session / claude session / project manager 都不能用"*, and §1 above had already cleared the server.
The client-side cause turned out to be shorter than anything in this document:

**`web/crates/zed_web_workspace/src/main.rs` never called `project_manager::init(cx)`,
`tmux_sessions::init(cx)` or `claude_sessions::init(cx)`.** `crates/zed/src/main.rs:743-746` calls
all three on the desktop. Each crate's `init` is the **only** registration of the `ToggleFocus` its
status-bar button dispatches, and `crates/workspace/src/dock.rs:1540` dispatches that action into a
tree where nothing handles it — which gpui drops silently.

So all three panels attached, constructed, **loaded their data** (`ProjectManagerPanel::new` calls
`reload()`, `TmuxSessionsPanel::new` calls `refresh()`) and drew their status-bar buttons with
correct tooltips. There was simply no way to open them. The observed signature — correct tooltip,
click produces no console output and no panel — is exactly what an unhandled action looks like.

**The web shell already knew this failure mode.** `main.rs:1404-1412` carries a comment explaining
that the desktop registers `AgentPanel`'s toggle actions in `zed.rs` rather than in `agent_ui::init`,
*"so the web shell must do"* it too or the *"panel can't be opened from the menu / keybinding"*. The
same omission was simply never noticed for this fork's own three panels.

`web/check-panel-actions.sh` now asserts the pairing. The lead hypothesis this round started
with — that `std::env::split_paths` was killing the panels' data loads — was **refuted with
evidence**: none of the three panels' data paths consults `PATH`. That panic is real but belongs to
`crates/project/src/git_store.rs:783/786`, where `which::which("git")` runs once per repository
opened, and is very likely killing the git backend in the same silent way.

## 5. A correction to the handoff's own method note

`docs/web-zed-handoff.md` §6 recommends: *"Make the success marker explicit, and look for it. The
check script prints `GUARD OK` as its last line. Three runs lacked it and still printed
`0 errors`; the absence was the signal."*

`GUARD OK` appears nowhere in `web/check-workspace-isolation.sh`, or in any script under `web/`.
The script ends with a count line and a bare `[[ "${failures}" -eq 0 ]]`. Two separate agents
this session were told to wait for that marker and correctly reported that it never comes.

The advice is good and the marker is not there. Either add it or stop citing it — a documented
success marker that does not exist is the same defect the note is warning about, wearing the
note's own clothes. The handoff has been corrected to say so.
