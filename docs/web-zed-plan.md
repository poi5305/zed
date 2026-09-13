# Web Zed — Porting Plan

Bringing a browser build of this fork online by adopting the community `zed-web` work,
without disturbing the desktop build, and with all four local features present.

- **Branch:** `andy/web-version` (created from `andy/project-manager-forward-ports-tmux` at `aaa5742d87`)
- **Reference implementation:** `zee295/zed`, branch `zed-web` (fetched locally as `zedweb/zed-web`)
- **Status:** research complete, no code written

## 0. How this document was produced

Four independent research passes, all read-only, none of which built anything:

| Pass | Scope | Agent |
| --- | --- | --- |
| A | Vendored dependency isolation | grok-4.6-xhigh |
| B | RPC surface × the four local features | grok-4.6-xhigh |
| C | Per-file classification of all 273 modified files | gemini-3.8-flash-high |
| D | Web crate architecture, protocol, auth model | this session |

Raw reports live outside the repo, in this session's scratchpad as `research/report-{A,B,C}.md`.

Claims below are marked **[verified]** when a command was run and its output read, and
**[unverified]** otherwise. Nothing here has been compiled for `wasm32-unknown-unknown`.

## 1. Decisions already taken

1. The web build gets its **own Cargo workspace**. Dependency overrides must not reach the desktop build.
2. **All four local features ship in the web build**: `claude_sessions`, `project_manager`,
   `forward_ports`, `tmux_sessions`. They are complementary to what `zed-web` provides, not redundant.
3. **Adopt the whole architecture at once** rather than a minimum viable slice.

## 2. What `zed-web` actually is

### 2.1 Size and currency [verified]

```
152 commits · 821 files · +157,312 / −1,888
548 added · 273 modified
merge-base with our upstream: fecc3273ed (2026-08-26)
last upstream sync: "Merge upstream stable v1.18.0" (2026-09-04)
```

The `1.13.0` in its published Docker tag is stale. The branch tracks upstream and ships
`web/sync-upstream.sh`, which rehearses the rebase in a throwaway worktree and only applies it
with `--apply`. `web/upstream-revision` records the sync point.

The `+157k / −1.9k` ratio is the important number: the work is almost entirely additive.

### 2.2 Architecture [verified]

It does **not** use Zed's SSH remote protocol. It defines its own:

| Crate | Role |
| --- | --- |
| `wasm_rpc` | The protocol. 529 lines, mostly a JS shim. |
| `zed_web_server` | Native `axum` HTTP + WebSocket backend. |
| `wasm_remote` | Browser side: implements Zed's existing `Fs` and git traits over RPC. |
| `zed_web_workspace` | Browser side: the wasm binary that assembles the app. |

`zed_web_server` splits by domain: `fs_rpc` `git_rpc` `process_rpc` `terminal_rpc`(portable-pty)
`sql_rpc`(rusqlite) `extension_rpc` `agent_rpc` `debug_adapter` `highlight_rpc` `auth` `auth_callback`.

The client protocol surface is tiny: `call<P,R>`, `call_void`, `on_notification`,
`subscribe_reconnect`, `is_connected`. The JS shim adds reconnect with exponential backoff and
jitter, an offline send queue, and a server-instance handshake that reloads the page when the
server restarts under it.

**The technique that makes this tractable**: the browser does not re-implement the UI. It
re-implements Zed's *existing trait boundaries* (`Fs`, git, process spawn, PTY, sqlite) on top of
RPC, so `editor`, `workspace`, `project` and the real desktop panels compile to wasm essentially
unchanged. From `load_core_panels`:

```rust
// Real desktop TerminalPanel: PTY I/O is RemotePty → server Terminal::*.
// Real desktop AgentPanel: native agent runs in-process over remote Fs +
// remote SQL; model providers stream over the wasm Fetch HTTP client.
```

`crates/zed/src/main.rs` is changed by exactly one line, and that line is unrelated to the web.
The desktop binary is untouched; `zed_web_workspace` is a separate binary that re-assembles the app.

### 2.3 Auth and exposure model [verified]

| Mechanism | Implementation |
| --- | --- |
| Token | `ZED_WEB_TOKEN`, compared with `constant_time_eq` |
| Session | HMAC-signed cookie, 30 days |
| Brute force | 5 failures per 5 min → 15 min block |
| CSRF | `same_origin()` header check |
| Default bind | `127.0.0.1:8090` |
| Path confinement | `ZED_WEB_RESTRICT_PATHS` (default **false**) |
| Local models | Built-in reverse proxy for ollama / llama.cpp / LM Studio |

Binding to loopback by default is the right shape for the intended deployment: Tailscale Serve
terminates TLS on the tailnet and proxies to `127.0.0.1:8090`. That also supplies the secure
context that `SharedArrayBuffer` — and therefore wasm threads — requires. Without it,
`gpui_web` degrades rather than fails: `WebDispatcher::new` checks `shared_memory_supported()`
and `wait_async_supported()` at runtime and falls back to a single-threaded dispatcher with a
warning (`crates/gpui_web/src/dispatcher.rs:164`).

### 2.4 Build shape [verified]

`web/build.sh` needs two toolchains: stable for the native server, **nightly** with
`-Z build-std=std,panic_abort` for wasm. It pins `wasm-bindgen-cli` by version and rejects a
mismatch. It sets a long `RUSTFLAGS` for wasm atomics (`+atomics,+bulk-memory,+mutable-globals`,
shared memory, 128 MB initial / 4 GB max), then post-processes the artifact with
`scripts/patch-wasm-bindgen-memory.sh`. It adds a `web-release` profile
(`opt-level = "z"`, `lto = "thin"`, `codegen-units = 1`).

It uses two target directories (`target/web-native`, `target/web-wasm`). We will keep them
separate from the desktop `target/` as well — see §9.

## 3. Architecture: the second workspace

### 3.1 The problem being solved [verified]

`zed-web` wires nine vendored forks in via workspace-level `[patch]` and path overrides in the
root `Cargo.toml`. Cargo's `[patch]` is read **only** from the workspace root manifest and has no
per-target form, so copying that wholesale would swap the desktop build's `tree-sitter`, `url`,
`smol`, `which`, `lsp-types`, `async-tar`, `alacritty_terminal` and `agent-client-protocol` for
forks. At least four of those are regressions against our tree (§4).

### 3.2 Chosen layout

```
Cargo.toml                    # desktop workspace — unchanged, no wasm patches
web/Cargo.toml                # virtual workspace; the nine [patch] entries live ONLY here
web/Cargo.lock
web/.cargo/config.toml        # wasm target rustflags only
web/vendor/{url,smol,which,…} # forks, members of the web workspace only
web/crates/zed_web_workspace/ # the wasm binary
crates/zed_web_server/        # native — stays in the ROOT workspace
```

Rules that make this work, each confirmed against the Cargo reference:

- Shared Zed crates (`editor`, `gpui`, `project`, `language`, …) stay members of the **root**
  workspace and are consumed by the web workspace as `path = "../crates/…"`. They must not be
  listed as members of both; Cargo does not support nested membership.
- `foo.workspace = true` inside a crate resolves against *that crate's own* workspace, so shared
  crates keep inheriting from the root table.
- `[patch]` applies to the entire dependency graph of **the build being run**. Building from
  `web/` resolves `tree-sitter` to the vendored fork; building from the root resolves it to git
  `43623ec`. This is the isolation point.
- Add `exclude = ["web"]` to the root workspace. A path dependency inside the workspace directory
  is otherwise auto-adopted as a member.
- `web/Cargo.toml` must carry its own `[workspace]` table, or `cd web && cargo build` walks up and
  joins the root workspace.
- The web workspace must re-declare the `[workspace.dependencies]` keys its own members use
  directly — not all 257, only what the web members name. It must also repeat any root
  `[patch.crates-io]` entry the web graph still needs (at minimum `tree-sitter-language`);
  patches do not cross workspaces.
- rustflags go in `web/.cargo/config.toml` under `[target.wasm32-unknown-unknown]`, never as
  `export RUSTFLAGS=` and never as `[build] rustflags`. Precedence is first-match-wins
  (`CARGO_ENCODED_RUSTFLAGS` > `RUSTFLAGS` > `target.*` > `build.*`), and `target.*` entries
  concatenate while the others replace. This repo already has a scar from that rule:
  `.cargo/bundle-config.toml`'s `[build] rustflags` replaces `.cargo/config.toml`'s, dropping
  `symbol-mangling-version=v0` and `tokio_unstable`.

### 3.3 Rejected alternatives

| Alternative | Why not |
| --- | --- |
| `[patch]` in `web/.cargo/config.toml`, single workspace | Config patches still rewrite the **root** `Cargo.lock`. Isolation fails. |
| Vendored crates as git submodules | The patches would still have to live at a workspace root; submodules do not change that. |
| `[target.'cfg(target_family="wasm")'.dependencies]` in the root | Cannot make `use smol::` resolve to a different crate of the same name. |
| `.cargo/config.toml` `paths = [...]` | Cannot alter graph structure; only valid for published crates. `tree-sitter` and `alacritty` are git sources. |
| Prove the nine forks native-equivalent and patch at the root | Measurement says at least four are not equivalent (§4). |

`zed_web_server` stays in the root workspace deliberately, so it never picks up the wasm forks
indirectly. It depends on `tokio`, `axum`, `rusqlite` and `portable-pty`, none of which need them.

## 4. Vendored dependencies — per-crate disposition

Nine forks, ~470 of the 548 added files. Do **not** copy them as a block. [verified]

| Crate | Upstream we use | What the fork changes | Take? |
| --- | --- | --- | --- |
| `agent_client_protocol_patch` | crates.io `=2.0.0` | `src/` is **byte-identical** to the registry across 51 files; the only diff is 4 `cfg` lines in `lib.rs` gating `acp_agent`/`stdio` | **Yes** — port the 4 lines onto our 2.0.0 |
| `url_wasm` | crates.io 2.5.7 (same checksum) | `+82 / −58`: wasm branches for `from_file_path`/`to_file_path`; rest is rustfmt | **Yes** — but check 2.5.8 first |
| `alacritty_terminal` | git `4c129667` | `home`/`libc`/`polling` and `event_loop`/`thread`/`tty` moved behind `cfg(not(wasm))`; the rest sampled as rustfmt noise | **Port the cfgs only**, onto `4c129667`. Do not import the reformatted tree. |
| `smol_wasm` | crates.io 2.0.2 | Not a fork — a full wrapper. Native `spawn` is hand-copied from upstream rather than re-exported | **Take the wrapper**, but make the native path `pub use` real smol |
| `wasm_thread_patch` | zed-industries git `0cf96c77` (we already fork it) | Native identical. wasm drops atomic-wait and injects a sqlez SQL-RPC SharedArrayBuffer bridge into the worker JS | **Port the worker JS onto our existing fork.** Do not swap the fork out. |
| `tree_sitter_wasm` | git `43623ec` | An **older** snapshot (`7f534862`). `build.rs` returns early on wasm32 and skips the C library; `wasm` feature no longer pulls wasmtime; C sources differ substantially | **No.** Redo the wasm decision on `43623ec`. |
| `lsp_types_wasm` | git `f1783e63` | Missing `DiagnosticMessage` (LSP 3.18), which we added in `12ef40c316` and `crates/diagnostics` uses | **No.** Port only the wasm `to_file_path` onto `f1783e63`. |
| `async_tar_wasm` | git `bd3ad6f` | Wrapper is fine; the bundled `real/` is crates.io 0.6.1 whose `pax.rs` **regresses** the embedded-newline fix and deletes its test | **Wrapper yes, `real/` no** — re-export our git instead |
| `which_wasm` | crates.io **8.0.5** | Pinned to **6.0.3**. We deliberately moved to 8 in `c595091b0a` to dedupe 6.0.3 + 4.4.2 | **No.** Write a wasm stub against 8.0.5, or use which 8's `Sys` trait. |

The pattern: the low-risk forks are thin `cfg` gates worth taking; the high-risk ones are stale
snapshots that would silently roll our tree backwards.

## 5. Porting the 273 modified files

### 5.1 Classification [verified]

| Category | Files | +lines | −lines |
| --- | ---: | ---: | ---: |
| `WASM_CFG` — pure cfg gates, native path unchanged | **174** | +5,863 | −1,266 |
| `MANIFEST` | 52 | +627 | −84 |
| **`API_BREAK` — touches the native path** | **18** | +960 | −369 |
| `FEATURE_GATE` / `TEST` / `OTHER` / `SEND_BOUND` / `TRAIT_WIDEN` / `TRIVIAL` | 29 | +1,473 | −169 |

64% is cfg gating. The classification total (+8,923) reconciles exactly with an independent
measurement (9,196 added lines minus 273 diff headers), so the counts are trustworthy.

### 5.2 Already present in our tree — not work [verified]

Three of the scariest `API_BREAK` entries are changes we already carry, so they vanish from the
real port:

| Entry | Our HEAD |
| --- | --- |
| `LanguageSettings::for_buffer -> Arc<…>` (was `Cow`) | present, `crates/language/src/language_settings.rs:280` |
| `BufferEvent::SettingsChanged` | present, `crates/language/src/buffer.rs:343` |
| `prettier_store::on_settings_changed` removed | already removed |

Note the provenance is *not* an upstream-merge artefact: all 152 commits are absent from our
`origin/main` (v1.18.0 is a release branch, not on main). The changes arrived by different routes.
What matters is the outcome: they are not work for us, and classifying them as high-risk was wrong.

### 5.3 Must NOT be taken

| File | Change | Why it must be refused |
| --- | --- | --- |
| `crates/zed/RELEASE_CHANNEL` | `dev` → `stable` | Changes logging, extension endpoints, and `script/bundle-mac`'s `bundle-${channel}` rewrite. This repo has already been bitten once by that code path leaving `crates/zed/Cargo.toml` mutated. |
| `crates/terminal/src/terminal.rs` | Shift+Click selection extension deleted | A real desktop regression, unrelated to wasm. |
| `crates/recent_projects/src/recent_projects.rs` | `PathPromptOptions.files: true` → `false` | The desktop project picker would stop accepting a single file. Also one of the five files we touch. |

### 5.4 Collision with our own work [verified]

Our 25 commits touch 64 files. Nine intersect `zed-web`'s modified set; four are trivial
(`.gitignore`, `Cargo.lock`, `Cargo.toml`, `README.md`). The five real ones are small on both sides:

| File | `zed-web` | ours |
| --- | --- | --- |
| `crates/recent_projects/src/recent_projects.rs` | +165 −80 | +1 |
| `crates/recent_projects/src/remote_connections.rs` | +2 | +20 −1 |
| `crates/remote/Cargo.toml` | +1 | +1 |
| `crates/settings/src/vscode_import.rs` | +2 | +1 |
| `crates/zed/src/main.rs` | +1 −1 | +4 |

This is small because our features are mostly new crates. There is no structural conflict.

## 6. The four local features

### 6.1 The governing fact [verified]

**`zed-web` builds a `local` Project, not a remote one.** `zed_web_workspace` calls
`workspace::open_paths`, which reaches `Project::local(…, app_state.fs, …)`; `remote_client()` is
`None`. "Local" here means GPUI believes the filesystem is local while every `Fs` call is an RPC
to `zed_web_server`.

The consequence is the central design constraint of this plan: **our panels will take their
local code paths, not the proto remote paths we already wrote for SSH.** `RemoteSource` will not
be selected. The existing client/server split is still an asset — but as *reusable server-side
functions*, not as an automatic transport.

Also: the four panels are registered only in `crates/zed`. `zed_web_workspace::load_core_panels`
loads Project/Outline/Git/Debug/Terminal/Agent and nothing else. Even with the I/O fixed, the UI
will not appear until that function registers them. `add_panel_when_ready<P: workspace::Panel>` is
generic, so each is roughly one line.

### 6.2 Correction to an earlier reading

A census of `crates/claude_sessions/src/` finds zero `std::fs` outside tests. That is true but
misleading: the disk and tmux I/O lives in `crates/remote/src/claude_sessions.rs`, which the crate
re-exports as `session_registry`, and which uses `std::fs` directly. `claude_sessions` is the
**hardest** of the four, not the easiest.

### 6.3 Shared prerequisite: a real HOME

`util/src/paths.rs:23` gates `home_dir()` behind `#[cfg(not(target_family = "wasm"))]`, so it does
not exist for wasm at all. `zed-web` hardcodes the wasm `home_dir()` to `"/workspace"`, which would
make `~/.claude` resolve to `<project>/.claude` and
`~/.config/zed/projects.json` to `/workspace/.config/zed/projects.json`.

There is no `Home::` method in the existing RPC. **A new RPC returning the server's real home and
config directories is a prerequisite for features 1 and 3.** It is small, and it should be done once.

A second shared concern: `ZED_WEB_RESTRICT_PATHS` defaults to false, which is what lets `fs_rpc`
read absolute paths outside the workspace. If a deployment turns it on, both `claude_sessions` and
`project_manager` need an allowlist.

### 6.4 Per-feature disposition

| Order | Crate | Effort | What it needs |
| --- | --- | --- | --- |
| 1 | `project_manager` | low–medium | Already goes through `Arc<dyn Fs>` (7 sites), which `zed-web` injects as `RemoteFs`, so the I/O works for free. Needs: server HOME/config dir; panel registration; VS Code import must pick paths by the **server's** OS rather than the wasm target's; `ssh://` / `wsl://` / `docker://` entries hidden or refused. No new RPC domain. |
| 2 | `tmux_sessions` | medium | `list` maps to `Process::output`, attach maps to `Terminal::open`. Needs `smol_wasm` merged, terminal remoting, panel registration. Verification cost exceeds coding cost — quoting and `;` handling must be tried against a real tmux. Failure mode: server has no `tmux`, for which the panel already has copy. |
| 3 | `claude_sessions` | high | `std::fs` reads of `~/.claude` must move to remoted I/O; needs the real HOME; `send_text` **must** use `Process::spawn` + `write_stdin` + `close_stdin`, because server-side `Process::output` does not write stdin at all; hook install must `chmod 0755` on the **server**; liveness detection (`/proc/<pid>/stat` on Linux, `ps` elsewhere) must run server-side or it will compile to the `not(unix)` empty branch and mark every session dead. Reuse the `remote::claude_sessions::*` functions server-side. |
| 4 | `forward_ports` | high | Not a wiring problem — the product semantics stop meaning anything (§6.5). |

### 6.5 `forward_ports` needs a new definition

Native semantics: bind `local_host:local_port` **on the machine running the Zed GUI**, tunnel to
`remote_host:remote_port` on the SSH host.

In a browser there is no "this machine" that anything can connect to. `smol_wasm`'s
`TcpListener::bind` returns `ErrorKind::Unsupported`, and even if it did not, a web page cannot be
an arbitrary TCP server. Meanwhile `zed-web`'s "remote" *is* the server the Project already lives on.

Proposed web semantics — **a listening port on the `zed_web_server` host, reachable as a URL the
browser can open**, i.e. the Codespaces port-preview model, not `ssh -L`.

| Requirement | Exists? |
| --- | --- |
| Enumerate forwardable listening ports on the server | Assemblable — `remote::listening_ports::scan_listening_ports` already handles Linux and macOS |
| Accept a browser connection on the server and bridge it to `localhost:port` | **No.** Needs an HTTP reverse proxy or a preview subdomain. `process_rpc`/`terminal_rpc` are not TCP stream multiplexers. |
| Bind `127.0.0.1:PORT` on the user's own machine | **No, and should not be built.** |

The settings UI for editing the forward list can stay. "Active / bound" has no meaning on the web,
and must not be displayed as though it does. Do not attempt to compile `PortForwardStore` to wasm.

### 6.6 Rejected: making `zed_web_server` a `HeadlessProject`

Tempting, because `remote_client()` would become `Some` and features 1–3 would light up through
the proto handlers we already wrote. Rejected because it means embedding a second Zed
`remote_server` (proto, entity subscriptions, `REMOTE_SERVER_PROJECT_ID`, session lifecycle) behind
axum, it conflicts with `zed-web`'s whole trait-substitution approach, and it **still does not fix
`forward_ports`**, whose missing half is the client-side bind. The cheaper path is to call the same
`remote::*` functions from the server over the JSON RPC.

## 7. Deployment

Target shape, no code changes required beyond the port itself:

```
iPad / MacBook Safari
  → https://<machine>.<tailnet>.ts.net     (Tailscale Serve, real Let's Encrypt cert)
  → 127.0.0.1:8090                          (zed_web_server, default bind)
```

Tailscale Serve supplies both the reachability and the secure context; it is a plain reverse proxy
and will not strip the app's own COOP/COEP headers. Inside a Coder workspace the same server can be
published as a `coder_app`.

iPad remains unproven and is explicitly not a gating requirement. The likely blocker is the
keyboard: Zed is keybinding-driven and `gpui_web` mediates text input through a hidden textarea
(`ime_mirror.rs`), which is the part most likely to behave badly with an iOS software keyboard.
Rendering is the least of the risks — `gpui_web` falls back to WebGL2 when WebGPU is absent
(`crates/gpui_web/src/platform.rs:249`).

## 8. Execution plan

### Phase 0 — measure the real rebase cost

Run `web/sync-upstream.sh` **without** `--apply` against our base, in a throwaway worktree, to get
the true conflict set rather than inferring it from the 152-commit diff. Nothing else should start
before this number exists. This is measurement, not porting.

### Phase 1 — the second workspace, empty

Stand up `web/Cargo.toml`, `web/.cargo/config.toml`, root `exclude = ["web"]`, and the duplicated
`[workspace.dependencies]` subset. Prove the desktop build is byte-identical (§9) **before** a
single vendored crate lands.

### Phase 2 — vendored dependencies, per §4

Take the four thin ones as `cfg` ports onto our own bases. Redo the `tree-sitter`, `lsp-types`,
`which` and `async-tar` decisions from scratch. Do not import the stale snapshots.

### Phase 3 — the 174 `WASM_CFG` files plus manifests

Mechanical. The three refusals in §5.3 must be filtered out, and the five collision files in §5.4
merged by hand.

### Phase 4 — `zed_web_server`, `wasm_rpc`, `wasm_remote`, `zed_web_workspace`

Mostly new files, so mostly copy. `zed_web_server` goes in the root workspace.

### Phase 5 — first light

Build, serve, open a project read-only in a desktop browser. This is the first point at which
anything is demonstrable.

### Phase 6 — the `Home::` RPC, then the four features in order

`project_manager` → `tmux_sessions` → `claude_sessions` → `forward_ports`, per §6.4. tmux wiring
must be verified before `claude_sessions`, which depends on the same machinery for its mirror
attach.

## 9. Proving the desktop build is uncontaminated

The commitment is that after every phase, the desktop dependency graph is unchanged. The check:

- The nine crates' `source` fields in the root `Cargo.lock` must stay exactly as they are today
  (`tree-sitter` git `43623ec`, `lsp-types` git `f1783e63`, `which` 8.0.5, `url` 2.5.7,
  `smol` 2.0.2, `async-tar` git `bd3ad6f`, `alacritty_terminal` git `4c129667`,
  `agent-client-protocol` 2.0.0 from crates.io, `wasm_thread` git `0cf96c77`).
- Root `Cargo.lock` must not gain a `web/` member or any `path` source pointing into `web/`.
- `cargo tree` for those nine, run from the root, must resolve to the same sources.
- The desktop bundle must still build and install.

Two standing rules, both learned the hard way in this repo:

- Never run `./script/clippy` or any other `--release --all-features` cargo command between
  `script/bundle-mac` runs. It alternates configurations in the shared `target/release` and has
  previously triggered a ~25 minute webrtc re-download. Use `cargo clippy -p <crate> --lib --tests`.
- The web build must never share `target/` with the desktop build. Different rustflags, different
  patches, different toolchains.

## 10. Open questions

None of these block starting, but each will need an answer before the phase that depends on it.

1. **Nothing has been compiled for wasm.** The expectation that the four crates fail to build
   inside `zed_web_workspace` today is an inference, not a compiler message.
2. Which `cfg` branch `process_start_times` selects on `wasm32-unknown-unknown`. If it is
   `not(unix)`, local session scanning silently reports every session as dead.
3. Whether `url` 2.5.8 already carries the wasm file-path support, which would remove one fork.
4. Whether `which` 8's `Sys` trait can replace the 6.0.3 stub outright.
5. Whether `tree-sitter` git `43623ec` can build for `wasm32-unknown-unknown` at all without
   `DEP_TREE_SITTER_LANGUAGE_WASM_HEADERS` — its `build.rs` panics when that is unset.
6. Whether `TMUX` and other environment variables survive `process_rpc`, which forwards only the
   env the client supplies.
7. Whether the four panels have native-only dependencies in their UI layers (icons, clipboard,
   notifications) that have not been surveyed.
8. What a `project_manager` "local path" means to a user in a browser: these are server paths, and
   there is no folder picker wired to that panel.

## 11. Risk register

| Risk | Severity | Mitigation |
| --- | --- | --- |
| A vendored fork silently rolls a dependency backwards | High | §4 disposition table; §9 lock-file check after every phase |
| `RELEASE_CHANNEL` change leaks into the desktop build | High | §5.3 refusal list; `script/bundle-mac` already demonstrated this failure mode |
| Rebase cost is larger than the diff suggests | Medium | Phase 0 measures it before any work starts |
| `forward_ports` semantics never settle | Medium | Scoped last; ship detection-and-URL before any tunnelling |
| Two `Cargo.lock` files drift | Medium | Both committed; §9 check is mechanical |
| Upstream rebases become expensive | Medium | `web/sync-upstream.sh` is adopted along with the code |
| iPad is unusable | Low | Explicitly not a requirement; MacBook is the target |
