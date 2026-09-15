# Web Zed — Porting Plan

Bringing a browser build of this fork online by adopting the community `zed-web` work,
without disturbing the desktop build, and with all four local features present.

- **Branch:** `andy/web-version`, rebased onto `andy/project-manager-forward-ports-tmux` at
  `dd850ebeb7`. It was first cut at `aaa5742d87`; §5.4, §5.5 and §6 were recomputed against the
  newer base, and the four commits between the two are what §5.5 exists for.
- **Reference implementation:** `zee295/zed`, branch `zed-web` (fetched locally as `zedweb/zed-web`)
- **Status:** Phase 0 and Phase 0b executed (2026-09-15); measurement reports in
  `docs/phase0-rebase-cost.md` and `docs/phase0b-wasm-report.md`. Both overturned something this
  document asserted — see §5.3's post-merge audit, Phase 0b's result block, and the top three rows
  of §11. No porting code written yet.

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
  `[patch.crates-io]` entry the web graph still needs; patches do not cross workspaces (the measured
  list is five entries, not one — see the `[patch]` bullet below).

  **The number is 97** [verified, Phase 4 research]. Parsed from the three web members'
  manifests: the union of keys they write as `.workspace = true` is 97 — **81** first-party root
  members reached by `path = "../crates/…"`, **14** third-party whose versions get copied from the
  root table, and **2** new web crates. `zed_web_workspace` alone names 79 of the 81. The root table
  has 511 keys, so the duplication §3.2 asks for is a fifth of it, not all of it.
- **`[profile.*]` must be re-declared too, and §3.2 originally missed it** [verified, round-1 review].
  Cargo honours profiles only at a *workspace root*. `zed-web` defines `[profile.web-release]` in the
  **root** manifest (`zedweb/zed-web:Cargo.toml:1099`); moving the wasm build into a second workspace
  drops it, and the first real build dies with `profile 'web-release' is not defined`. The obvious
  fix — add it to our root manifest — pushes a wasm-only profile into the desktop workspace, so it
  belongs in `web/Cargo.toml`. This is the third "must re-declare" item, alongside
  `[workspace.dependencies]` and `[patch]`.
- **The `[patch]` list is not a guess and is larger than one entry** [verified]. The intersection of
  the root `[patch.crates-io]` keys with what the wasm graph actually reaches —
  measured with `cargo metadata --filter-platform wasm32-unknown-unknown` from `gpui`, `scheduler`,
  `rpc`, `remote`, `project` and `editor` — is `tree-sitter-language`, `async-process`, `async-task`,
  `notify` and `notify-types`. `async-task` is reachable by three independent paths. Omitting one
  fails **silently**: the unpatched version resolves, compiles, and behaves differently.
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

### 4.1 The wall Phase 0b hit needs **zero** additional forks [verified]

`docs/phase2-dependency-wall.md` traced every crate that blocked the wasm probe back to how
`zed-web` removes it. The answer is reassuring for Phase 2's scope: **the nine forks above are the
complete fork list.** The other six blockers are cleared without vendoring anything, mostly by
pruning the graph rather than patching it:

| Blocker | How it actually disappears |
| --- | --- |
| `errno` | Not gated directly — **`smol_wasm` drops `async-io`/`async-process` on wasm** (`crates/smol_wasm/Cargo.toml:15-20` [verified]), and the edge `async-io → polling → rustix → errno` goes with it. **Confirmed on our own graph once the real crates landed** (§4.3): `async-io`, `async-process`, `errno`, `rustix` and `async-net` are all absent, and only one `smol` instance exists — the vendored one. The mechanism works. |
| `polling` | **The `smol_wasm` edge is not the only one.** `alacritty_terminal` depends on `polling` directly, and that needs the §4 cfg port — see §4.3. |
| `wasmtime` | `zed_web_workspace` **never depends on `extension_host`** (0 references [verified]); the other edge dies when tree-sitter's `wasm` feature stops pulling wasmtime |
| `zstd-sys` | Its three wasm-reachable users move to `[target.'cfg(not(target_family = "wasm"))'.dependencies]` |
| `trash` | Moved native-only in `crates/fs/Cargo.toml:45-53`; `fs` stays in the graph, it just stops naming `trash` |
| `getrandom` 0.2 | A wasm-only dummy dep unifies the `js` feature onto every 0.2 user: `crates/rpc/Cargo.toml:43` `getrandom_02 = { package = "getrandom", version = "0.2", features = ["js"] }` [verified] |
| `getrandom` 0.3 | Needs **both** the `wasm_js` feature and `--cfg getrandom_backend="wasm_js"`; neither alone is enough |
| `tree-sitter-json` | Gated native-only on most consumers + `languages = { default-features = false }`, but **`tasks_ui` still uses it on wasm** — which is what forces the WASI SDK |

So §4's disposition table stands. What §4 **omitted** is not forks but build inputs and wiring:

1. **WASI SDK v25 is a required wasm build input** (`web/build.sh:10,47-51`, `script/download-wasi-sdk`),
   supplying `CC_wasm32_unknown_unknown` and `CFLAGS_…=-isystem <wasi-sysroot>/include/wasm32-wasi`.
   This is the `'stdlib.h' file not found` fix — and the reason open question 5's answer needed its
   qualifier. Note it is **not** what fixes `tree-sitter` core: that is the fork's `build.rs`
   returning early on `wasm32` (`binding_rust/build.rs:15-17` [verified]).

   **And it is far more load-bearing than "one grammar for `tasks_ui`"** [verified, round-3 review
   plus brain follow-up]. See §4.2.
2. A list of target-cfg moves in first-party manifests (`fs`, `rpc`, `agent`, `edit_prediction`,
   `language_models`, `extensions_ui`, `agent_ui`, `settings`, `debugger_ui`, `keymap_editor`,
   `settings_json`) — Phase 3 `MANIFEST` work, now with exact sites.
3. ~~`languages = { path = …, default-features = false }` at workspace level, so the web binary never
   enables `load-grammars`.~~ **Both halves of this are wrong — see §4.2.**
4. `smol_wasm`'s load-bearing job for *this* wall is dropping `async-io`, not the RPC stubs §4
   described. Do not scope it as "just a wrapper".

### 4.3 The wall, re-measured on **our** graph once the real crates landed [verified]

§4.1's disposition table was traced through `zed-web`'s manifests. Phase 4b put `wasm_rpc`,
`wasm_remote` and `zed_web_workspace` into `web/` for the first time, which is the first moment the
question could be asked of *our* workspace — and the answer is not the same.

`cargo metadata --filter-platform wasm32-unknown-unknown`, run from `web/`:

| Crate | In our wasm graph? | Why |
| --- | --- | --- |
| `async-io`, `async-process`, `errno`, `rustix`, `async-net` | **no** | `smol_wasm` works. Only one `smol` instance resolves, the vendored path one. |
| `polling` | **yes** ← `alacritty_terminal` | A second edge §4.1 never traced. Fatal: `polling` is a `compile_error!` on wasm32. |
| `blocking` | **yes** ← `async-fs` ← `crates/languages` | Not a defect — see below. |

Two lessons, both about how the earlier measurements were framed:

1. **`polling` has two independent paths into the graph**, and §4.1 only closed one. The second is
   `alacritty_terminal`'s direct `polling` dependency, which `zed-web` gates at
   `crates/alacritty_terminal/Cargo.toml:27` under `cfg(not(target_family = "wasm"))` alongside
   `home` and `libc`. §4's disposition for that crate already says "port the cfgs only, onto
   `4c129667`" — Phase 2 simply had not reached it yet, having taken only the four thin forks. So
   this is scheduled work surfacing on time, not a surprise.
2. **The `blocking` entry was an over-strict assertion, not a breach.** `crates/languages` declares
   `async-fs.workspace = true` ungated — and so does `zed-web`'s copy of that file, byte for byte.
   It is in their wasm graph too. The gate's crate list was validated in round 2 against a
   183-package graph containing only the four vendored crates; it did not survive the graph getting
   real. **Re-derive such lists against the graph you actually ship, not a stub of it.**

A third failure surfaced in the same run and is genuine: `crates/net` imports `smol::net::unix`,
which our `smol_wasm` deliberately does not provide. The fix is to gate `net`, **not** to add the
module back to `smol_wasm` — doing that would re-admit real `smol` and take `async-io`, `polling`
and `errno` with it, undoing the one mechanism §4.1 proved works.

### 4.2 Ruling: the web build **does** compile 18 tree-sitter grammars, and WASI is what allows it

The round-3 review measured that `languages = { default-features = false }` is a **provable no-op**:
`cargo tree -e features` with and without it differs by two lines of an empty synthesised node, and
the lock is byte-identical. The reason is simple — **`crates/languages` declares no `default`
feature at all**, only `test-support` and `load-grammars` [verified]. Turning off a default that does
not exist changes nothing. `zed-web` carries the same line; it is cargo-cult copied from upstream,
harmless, and we keep it only for parity.

The consequential half is the *rationale*, and chasing it down inverts the picture:

- `load-grammars` is **enabled in the wasm graph**, not excluded from it. `crates/markdown:51` and
  `crates/edit_prediction:83` both declare `languages = { workspace = true, features =
  ["load-grammars"] }` — and `zed-web`'s copies of those two files are **identical to ours**
  [verified]. Cargo unifies features across the graph, so one enabler is enough.
- `zed_web_workspace` reaches `markdown` by at least three routes: `editor`, `markdown_preview`,
  `agent_ui` [verified].
- `grammars`' `load-grammars` feature pulls **18 `tree-sitter-*` crates** — bash, c, cpp, css, diff,
  gitcommit, go, go-mod, gowork, jsdoc, json, md, python, regex, rust, typescript, yaml, plus
  `tree-sitter` itself [verified].

So the honest statement is the opposite of §4.1's item 3: **the web build compiles 18 grammar C
libraries for `wasm32-unknown-unknown`, and the WASI SDK's clang + `wasi-sysroot` headers are the
only reason that is possible.** `tasks_ui`'s single `tree-sitter-json` use is a footnote, not the
cause.

Three consequences to plan against:

1. **WASI SDK is a hard prerequisite of Phase 4/5**, not an optional convenience. `web/build.sh`'s
   port must fetch and export it, and §2.4's build-shape summary should say so.
2. **The Phase 0b C-build failures were never going to be fixed by manifests.** `tree-sitter-json`'s
   `'stdlib.h' file not found` was the host clang talking; it recurs for all 18 until the WASI
   toolchain is wired.
3. If we ever *want* the smaller graph, the lever is `markdown` and `edit_prediction`, not
   `default-features`. That is a product decision (no syntax highlighting in rendered markdown on
   the web) and is **not** being taken here.

**One architectural caveat, and it is ours to own:** `zed-web` puts its `[patch]` tables in the
**root** `Cargo.toml` and builds `-p zed_web_workspace` from the root workspace. §3.2 deliberately
rejects that for isolation. The second workspace remains the right call for protecting the desktop
build, but it means none of `zed-web`'s build wiring transfers verbatim — every `[patch]`, feature
unification and `default-features` decision above has to be re-expressed in `web/Cargo.toml`, where
feature unification works over a *different* crate set. That is the main thing Phase 2 has to prove.

### 4.4 The grammar question, measured three times and wrong twice [verified]

This section has been rewritten twice, and the shape of the two errors is worth more than
the answer, because both were the same shape: **each time the question asked was "does this
tool work?" when the question needed was "is this the right tool?"**

| Draft | Claim | Why it was wrong |
| --- | --- | --- |
| §4.2 | "The web build compiles 18 grammar C libraries, so the WASI SDK is a hard prerequisite." | The count was wrong — `load-grammars` was off, and the graph held exactly one grammar, pulled by `tasks_ui`. The prerequisite was right for the wrong reason. |
| §4.4 draft 1 | "The grammars cannot build for wasm32 even with the WASI SDK, so the browser has no syntax highlighting — a BLOCKER with three expensive ways out." | The WASI sysroot was never the right headers. Its `<wasi/api.h>` refuses any target that is not WASI proper, and `wasm32-unknown-unknown` is not one. None of the three proposed routes was needed. |

**What is actually true**, measured: tree-sitter vendors the libc subset its parsers need for
this exact target and publishes the headers from its `language` crate. Its own
`src/wasm-stdlib/README.md` says so plainly — *"when the Tree-sitter Rust library is
compiled for `wasm32-unknown-unknown`, the same vendored libc sources ... are linked directly
into the application"*. Point `CFLAGS_wasm32_unknown_unknown` at
`<tree-sitter-language>/wasm/include` instead of the WASI sysroot and the C compiles:
`tree-sitter-json` and `tree-sitter-rust` both build, and `keymap_editor` keeps its syntax
highlighting on the web. `web/build.sh` derives that path from `cargo metadata` rather than
hard-coding it, since it lives under `~/.cargo/git`.

The WASI SDK is still the *compiler* (`CC_wasm32_unknown_unknown`); it is its *sysroot* that
was the wrong choice.

#### What still stops the other sixteen

`load-grammars` remains off, but no longer for an architectural reason. Enabling it fails on
two things that have nothing to do with the toolchain:

1. `tree-sitter-bash` 0.25.1 and `tree-sitter-c` 0.24.2 reach
   `tree-sitter-language`'s deliberate compatibility `#error`: *"tree-sitter 0.26 is
   incompatible with this version of tree-sitter-language on wasm32-unknown-unknown; upgrade
   tree-sitter to 0.27 or newer"*. These grammar versions predate the mechanism.
2. Some scanners rely on the host's headers being transitively included — `tree-sitter-bash`'s
   calls `isdigit` without including `<ctype.h>`, which a freestanding libc does not forgive.

Both are per-grammar and bounded, and neither requires changing the desktop build. This is a
much better-specified problem than the blocker it replaces: **the browser can have syntax
highlighting; what it needs is grammar version bumps, not an architecture.**

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
| `crates/recent_projects/src/recent_projects.rs` | `PathPromptOptions.files: true` → `false` | The desktop project picker would stop accepting a single file. Also one of the collision files in §5.4. |
| `crates/remote_server/src/server.rs` | log flush `send_blocking` → `try_send` | `remote_server` is native-only and is not in the wasm graph at all, so this buys the web build nothing. What it costs the desktop build is a lossy log: `try_send` returns an error on a full channel where `send_blocking` waited. An unconditional native behaviour change, same class as the two above. |

#### These do not announce themselves [verified]

Phase 0 measured where each of the four actually lands, and the answer changes how they have to be
handled: **three of the four do not conflict.** They auto-merge, silently, into the merged tree.

| Refusal | Conflicts? | Where it lands | Action |
| --- | --- | --- | --- |
| `RELEASE_CHANNEL` `dev`→`stable` | no | merged blob is `stable` | restore `dev` |
| `terminal.rs` Shift+Click deleted | no | deletion merges cleanly | restore the block |
| `recent_projects.rs` `files: true`→`false` | file conflicts elsewhere (dev-container hunk), **but `files: false` auto-merges** at merged line 2164 | merged blob is `false` | restore `true` at `open_local_project` |
| `server.rs` `send_blocking`→`try_send` | no | merged blob is `try_send` at line 255 | restore `send_blocking` |

A refusal list is only useful during a merge if the merge stops on it. This one does not. So it is
not a filter applied *while* merging — it is a **post-merge audit**, four exact assertions run
against the merged tree after Phase 3 and re-run after every `sync-upstream`. Four greps is cheaper
than one regression that reaches a release build.

**Written and landed: `web/check-refusals.sh`** ✅ — 4 assertions, 0 failures on the current tree,
exit non-zero on failure, same `ok`/`FAIL` output shape as `web/check-workspace-isolation.sh`. Each
was proved non-vacuous by breaking that one item, watching only it go red, and restoring. Two
details worth keeping:

- §5.3.3 cannot be a plain grep. `recent_projects.rs` has **two** `PathPromptOptions` sites and
  `zed-web` only changes the one in `open_local_project`; the other (Open WSL folder) is legitimately
  `files: true` on both sides. The assertion brace-matches the `open_local_project` body and reads
  only the options block inside it, so flipping the wrong site is still caught.
- The assertions bind to behavioural strings, not line numbers, because Phase 3's merge moves lines.

Run it after Phase 3, after Phase 4, and after every upstream sync — not once.

### 5.4 Collision with our own work [verified]

Our 29 commits touch 69 files. Twelve intersect `zed-web`'s modified set; four are trivial
(`.gitignore`, `Cargo.lock`, `Cargo.toml`, `README.md`). The eight real ones are small on at least
one side of every row:

| File | `zed-web` | ours |
| --- | --- | --- |
| `crates/recent_projects/src/recent_projects.rs` | +165 −80 | +1 |
| `crates/project/src/project.rs` | +34 −12 | +5 |
| `crates/remote/src/remote_client.rs` | +2 −1 | +464 −5 |
| `crates/recent_projects/src/remote_connections.rs` | +2 | +20 −1 |
| `crates/settings/src/vscode_import.rs` | +2 | +1 |
| `crates/remote/Cargo.toml` | +1 | +1 |
| `crates/zed/src/main.rs` | +1 −1 | +4 |
| `crates/remote_server/src/server.rs` | +1 −1 | +14 −3 |

This is small because our features are mostly new crates. There is no structural conflict.

The three rows added since the first count are the ones where our side is now much the larger, and
none of them is harder for it:

- `remote_client.rs` — `zed-web`'s whole change is swapping `std::time::Instant` for
  `web_time::Instant` in the import block at the top of the file. Our +464 is far below it.
  See §5.5.
- `remote_server/src/server.rs` — the two hunks are ~370 lines apart (theirs at 252, ours at 620),
  and theirs is refused outright by §5.3.
- `project.rs` — `zed-web`'s change is the `terminals` / `terminals_wasm` module swap described in
  §6.4; ours is five lines elsewhere in the file.

### 5.5 A standing rule for code we write from here on: `Instant` [verified]

`std::time::Instant::now()` panics on `wasm32-unknown-unknown`. `zed-web`'s answer is mechanical and
pervasive: **45 files swap `std::time::Instant` for `web_time::Instant`**, `crates/remote/Cargo.toml`
among the manifests that gain the dependency. It is not called out anywhere in §5.1's classification
because it is spread across the `WASM_CFG` bucket, which is exactly why it needs saying here.

Our tree already half-agrees: `crates/scheduler/src/clock.rs:5` is `pub use web_time::Instant`, so
`BackgroundExecutor::now()` already returns a `web_time::Instant`. On native the two types are the
same type — `web_time` re-exports `std::time` off wasm — so mixing them compiles today and says
nothing.

We have already planted one instance of the mismatch. `crates/rpc/src/proto_client.rs:21` imports
`std::time::{Duration, Instant}`; `:120` stores `cx.background_executor().now()` into
`queued_early_messages_since: Option<Instant>`, with `:184` and `:231` on the same path.
Native-clean, wasm-broken, and `crates/rpc` is in the wasm graph — `zed-web` adds `wasm_conn.rs` to
it and rewrites `peer.rs` and `message_stream.rs`.

`zed-web` already ships the gate for exactly this, and we should adopt it with the code:
`web/check-wasm-time.sh` walks `cargo tree -p zed_web_workspace --target wasm32-unknown-unknown`,
greps every reachable crate for `std::time::Instant`, and diffs the inventory against
`web/wasm-std-instant.allowlist` (47 entries). `crates/rpc/src/proto_client.rs` is **not** in that
allowlist, so the gate would fail on our tree today. Treat a new allowlist entry as something that
must be argued for — the script's own wording is "update the allowlist only after proving the
occurrence is excluded from WASM by cfg or is test/fixture-only". The gate needs
`zed_web_workspace` to exist, so it cannot run before Phase 4.

Phase 0b proved the split is real rather than theoretical [verified], by compiling the same pattern
in an isolated crate outside the repo: native `cargo check` succeeds; `--target
wasm32-unknown-unknown` fails with `error[E0308]: mismatched types … expected
`std::time::Instant`, found `web_time::Instant``. It did **not** catch `proto_client.rs` itself,
for the reason Phase 0b records: `crates/rpc` never type-checked at all.

The fix is one import line. The rule is what matters:

> In any crate that reaches the wasm graph, spell the type `web_time::Instant`, never
> `std::time::Instant`. Prefer taking the value from `BackgroundExecutor::now()`, which is already
> the right type and is the only one a GPUI test can advance.

`SystemTime` has the same hazard and no `scheduler` equivalent; nothing in our tree uses it on a
wasm-reachable path yet, and nothing should start.

### 5.6 What the `Instant` gate found once it could run [verified]

`web/check-wasm-time.sh` was written in Phase 3b and believed green through two review
rounds. It was neither green nor red: it resolved `-p zed_web_workspace` against the **root**
manifest, and the root sets `exclude = ["web"]`, so every run ended at

```
error: package ID specification `zed_web_workspace` did not match any packages
```

It exited non-zero, so it was not silently passing — but nothing had ever consulted it, and
"needs Phase 4" in the handoff was the wrong diagnosis. Pointed at `web/Cargo.toml` it works,
and the first real run was red on ten counts:

| Finding | Meaning |
| --- | --- |
| `client.rs`, `editor/cursor_animation.rs`, `editor/element.rs`, `acp_thread/terminal.rs`, `edit_prediction_context.rs` | live `Instant::now()` in the wasm graph — compiles, **panics in a browser**, invisible to every native test |
| `alacritty_terminal` × 3 | stale allowlist entries; the crate moved to `web/vendor/` in Phase 2 |
| `editor/scroll.rs` × 2 | stale allowlist entries; fixed during the port |

Two process lessons, both cheap to state and expensive to relearn:

1. **A gate that has never been red has not been shown to work.** This one was authored,
   reviewed twice, and recorded as a standing guard while being incapable of checking anything.
   Every new gate needs one deliberate red run against a known-bad input before it is trusted.
2. **The gate matches comments too**, so a note mentioning the forbidden path trips it. That is
   the right trade: a gate that over-reports costs a reworded comment, while teaching it to skip
   comments risks teaching it to skip a real occurrence inside a string. Reword the comment.

### 5.7 Phase 4 is not done, and the compiler could not say so until now [verified]

With every other crate in the graph compiling, `cargo check --workspace --target
wasm32-unknown-unknown` finally reaches `zed_web_workspace` and reports 25 errors. Nine of
them are calls to things that **do not exist anywhere in the tree** — verified by searching for
each definition, not inferred from the error text:

| Missing | Shape |
| --- | --- |
| `sqlez::remote_sql::{set_sql_endpoint, set_sql_rpc_endpoint, set_async_sql_client}` | an entire module: workspace/KVP persistence routed to the server's SQLite |
| `db::prepare_web_database` | its async initialisation, awaited in `try_join!` at startup |
| `assets::install_web_assets` | installs fetched assets into the embed store |
| `terminal::set_remote_client` | the terminal remoting hook §6.4.1 depends on |
| `settings::default_keymap_path`, `settings::specific_overrides_keymap_path` | keymap path accessors |
| `Session::for_web` | a session that is not backed by a local sqlite file |
| `Workspace::initial_state_loaded` | |
| `PlatformTitleBar::set_left_padding` | |

Two more of the same kind were found and fixed while getting here:
`extensions_ui::init_remote_store` was called once and defined nowhere, and
`web_extensions.rs` — 512 lines, documented as the web extensions UI over the `Extensions::*`
RPC — was never declared as a module, so it had never been compiled at all.

**The lesson is structural, not clerical.** An entry-point crate that nothing else depends on
is the last thing a workspace check reaches, so every error in front of it hides every error
inside it. Phase 4 could be written, reviewed and recorded as complete while referring to an
API surface that was only ever planned. A crate in that position needs its own `cargo check`
from the day it is created, even when — especially when — it cannot yet link.

#### Ruling — SQL goes over RPC, the workspace layout does not go over SQL

This section was written twice before it was right, and both wrong versions failed the same
way: **the thing being measured was one level above the thing that does the work.** Recording
that is more useful than recording the answer.

- *First draft:* "`sqlez` is synchronous and the transport is async, so this is an
  architectural fork needing workers and `Atomics.wait`." **Wrong.** `db`'s `query!` macro
  wraps `sqlez`, and its dominant form generates `async fn` bodies. Callers already await.
- *Second draft:* "therefore the layout persists over `Sql::` RPC." **Also wrong.**
  `save_workspace` — the function that writes the pane and tab layout — does not use
  `query!` at all. It is a hand-written `self.write(|conn| conn.with_savepoint(…))`
  transaction with branching and recursive pane-group inserts inside the closure, so there is
  no static SQL to forward.

Measured, on `crates/workspace/src/persistence.rs`: **5 hand-written `self.write` closures**
(`save_workspace`, `get_or_create_remote_connection`, `toolchains`, `set_toolchain`,
`save_trusted_worktrees`), **1 `with_savepoint`**, and **18 direct `conn.select`/`conn.exec`
reads**. The layout is written by the first of those and read by several of the last.

The ruling, in two parts:

| Path | Where it runs |
| --- | --- |
| `query!`'s **async** arms | over `Sql::query`/`Sql::batch` to the server's real SQLite. Every caller already awaits, so none change. Covers KVP and most workspace queries |
| `query!`'s **sync** arms | fail honestly on wasm. They carry debugger breakpoints and worktree trust, not layout |
| **the workspace layout itself** | a `Workspace::` RPC, **not** the SQL layer |

The last row is the part worth arguing for. `zed_web_server` already serves
`Workspace::ui_state`, `Workspace::activate`, `Workspace::set_sidebar_open` and
`Workspace::set_project_groups`, so workspace state is already half server-side; layout joins
what is there. More importantly, `save_workspace`'s transaction then runs **unchanged**, on a
machine that has a synchronous connection, instead of being re-expressed as a batch and
trusted to still be equivalent. Nothing in the browser blocks, nothing needs
`SharedArrayBuffer`, and the layout follows the user to another machine.

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

The number, because it is what the schedule hangs on: `crates/remote/src/claude_sessions.rs` has
**113 `std::fs` / `fs::` sites** [verified]. And `crates/remote` is squarely in the wasm graph —
`zed-web` adds `web-time` to its manifest and rewrites `remote_client.rs` and all three transports
so the file cannot simply be left to fail; it has to compile for `wasm32`, whether by remoting each
site or by `cfg`-gating the module out of the wasm build and reaching it only server-side.

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
| 2 | `tmux_sessions` | medium | `list` maps to `Process::output`, attach maps to `Terminal::open`. Needs `smol_wasm` merged, terminal remoting, panel registration, and the `terminals_wasm` check below. Verification cost exceeds coding cost — quoting and `;` handling must be tried against a real tmux. Failure mode: server has no `tmux`, for which the panel already has copy. |
| 3 | `claude_sessions` | high | `std::fs` reads of `~/.claude` must move to remoted I/O; needs the real HOME; `send_text` **must** use `Process::spawn` + `write_stdin` + `close_stdin`, because server-side `Process::output` does not write stdin at all; hook install must `chmod 0755` on the **server**; liveness detection (`/proc/<pid>/stat` on Linux, `ps` elsewhere) must run server-side or it will compile to the `not(unix)` empty branch and mark every session dead. Reuse the `remote::claude_sessions::*` functions server-side. |
| 4 | `forward_ports` | high | Not a wiring problem — the product semantics stop meaning anything (§6.5). |

### 6.4.1 `terminals.rs` is replaced on the web, not gated [verified]

Worth stating separately, because "terminal remoting" makes it sound like one substitution deep in
the PTY layer. It is not. `zed-web` leaves `crates/project/src/terminals.rs` alone and writes a
parallel **`crates/project/src/terminals_wasm.rs` (280 lines against our 920)**, then swaps the
module in `project.rs`:

```rust
pub mod terminals;
pub mod terminals_wasm;
use terminals_wasm as terminals;   // wasm only
```

Two consequences, both for `tmux_sessions`:

1. **Nothing we add to `terminals.rs` reaches the web build.** `5c69df78fd` put a timeout on
   `directory_environment` there, so a terminal whose shell environment never arrives opens with
   what is already known instead of waiting forever. The web build has no counterpart. Whether that
   bug exists in `terminals_wasm.rs` at all, and whether the fix should be repeated there, is an
   open decision rather than free carry-over.
2. **The attach path looks present, but that is an assumption to test, not a given.** Our panel
   calls `TerminalPanel::spawn_task` (`tmux_sessions_panel.rs:229`) with a `SpawnInTerminal`, and
   `terminals_wasm.rs` does expose `create_terminal_task` (:56) alongside `create_terminal_shell`,
   `restore_terminal_shell`, `create_local_terminal` and `clone_terminal`. Confirming that
   `spawn_task` reaches `create_terminal_task` on the wasm path is the first thing to check in
   feature 2, before any tmux quoting work.

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

### Phase 0b — ask the compiler what it thinks of our own crates

Cheapest measurement in the plan, and it can run beside Phase 0. Nothing in this document has been
compiled for `wasm32-unknown-unknown` (§11, first row), so every "this will port" is read off source
rather than reported by a compiler. One class of breakage — the `Instant` mismatch in §5.5 — is
invisible on native by construction: the types are the same type off wasm. There is no way to know
how many more of those exist except to ask.

Point a `wasm32-unknown-unknown` `cargo check` at the two crates we have changed the most and that
`zed-web` proves are in the wasm graph:

```
CARGO_TARGET_DIR=target/web-probe cargo check -p rpc    --target wasm32-unknown-unknown
CARGO_TARGET_DIR=target/web-probe cargo check -p remote --target wasm32-unknown-unknown
```

`CARGO_TARGET_DIR` is not optional. §9's second standing rule — the web build never shares `target/`
with the desktop build — applies to a probe as much as to a build, and this repo has already paid
once for alternating configurations in a shared target directory. The host-side build scripts and
proc macros this compiles are native artefacts, and they land in the shared `target/debug` without
it. `wasm32-unknown-unknown` is already installed [verified], and `cargo check` needs neither
nightly nor `-Z build-std` — but see the result below: that is not the same as needing nothing else.

It will fail, and that is the deliverable: an error count and an error *taxonomy*, not a build.

#### Result — run 2026-09-15, `docs/phase0b-wasm-report.md` [verified]

**The probe could not see our own code at all, and that is the finding.** Both invocations died on a
third-party `compile_error!` before type-checking a single line of `crates/rpc` or `crates/remote`:
`rpc` on `getrandom` 0.2.16 (11s), `remote` on `errno` 0.3.14 (26s). Two diagnostics each, both
class (a). `claude_sessions.rs` contributed **0** errors, not the predicted large block — the file
was never compiled, so the 113-site census in §6.2 is neither confirmed nor denied.

The paragraph this replaces claimed "there is nothing else to set up". That was wrong, and it was
wrong in the most useful way: **there is a dependency wall in front of every §6 estimate**, and a
`--keep-going` follow-up mapped it. Beyond §4's nine forks the wall also holds `getrandom` 0.2 *and*
0.3 (the latter needs a `RUSTFLAGS` cfg, not just a feature), `errno`, `polling`, `zstd-sys`,
`tree-sitter-json`, `trash` and `wasmtime`. §4's list is therefore **incomplete**, which is what
Phase 2's real scope has to be measured against.

What the probe did buy, all of it compiler-spoken rather than inferred:

- **Open question 2 — answered, and badly.** `rustc --print cfg --target wasm32-unknown-unknown`
  emits `target_os="unknown"`, `target_family="wasm"`, and **no `unix`**. So of
  `claude_sessions.rs`'s three arms (`:205` linux, `:236` unix-not-linux, `:278` `not(unix)`), wasm32
  selects the `not(unix)` empty map — confirmed by a `compile_error!` probe on the same three
  predicates. In a browser, Claude session liveness would mark **every session dead, silently**.
- **Open question 5 — answered: no.** `tree-sitter` git `43623ec` does not build for
  `wasm32-unknown-unknown`; its C build fails in `lib/src/wasm_store.c:315`.
- **§6.3's HOME hole, spoken by rustc.** The one class-(b) error the compiler did emit is
  `error[E0432]: unresolved import` at `crates/paths/src/paths.rs:8`, re-exporting a `home_dir`
  that `crates/util/src/paths.rs:23` configures out for wasm.
- **§5.5's type split is real**, proved in an isolated crate rather than claimed.

Our crates that *did* type-check for wasm32 on the way down: `refineable`, `scheduler`, `rope`,
`clock`, `text`, `util`, `path`, `sum_tree`, `http_client`, `collections`, `zlog`, and others.
`gpui` was still compiling when the run aborted, so it is **not** known to pass.

#### Consequence for the plan's ordering

Phase 0b as written cannot run before Phase 2, and probably not before Phase 3 — the manifests are
part of how `zed-web` keeps these crates out of the wasm graph. Re-run it as a **gate at the end of
Phase 3**, where the same two commands become a real measurement of our own code rather than of the
vendor wall. Until then, §6's effort column stays unvalidated (§11, first row).

### Phase 1 — the second workspace, empty ✅ DONE (2026-09-15)

Stand up `web/Cargo.toml`, `web/.cargo/config.toml`, root `exclude = ["web"]`, and the duplicated
`[workspace.dependencies]` subset. Prove the desktop build is byte-identical (§9) **before** a
single vendored crate lands.

**Landed and independently verified** — `docs/phase1-workspace.md`, plus a re-check by the brain
rather than a reading of the agent's own report:

| §9 assertion | Result |
| --- | --- |
| Root `Cargo.lock` unchanged | byte-identical to the pre-change snapshot |
| Root `Cargo.toml` minimal | exactly one line added, `exclude = ["web"]` |
| `web/` is its own workspace | `workspace_root` = `…/zed/web`, 0 members, `target_directory` = `web/target` |
| Root workspace intact | 257 members, resolves clean |
| `resolver` consistent | `"2"` in both |

`web/.cargo/config.toml` carries the wasm rustflags under `[target.wasm32-unknown-unknown]` as §3.2
requires, and they match `zed-web`'s `web/build.sh:45` flag for flag — including
`--cfg getrandom_backend="wasm_js"`, which is the documented fix for one of the `getrandom` blockers
Phase 0b hit. Note the deliberate divergence: `zed-web` sets these with `export RUSTFLAGS=` and
builds `-p zed_web_workspace` **from the root workspace**; §3.2 rejects both, so `web/build.sh`
cannot be adopted verbatim and will need a port.

### Phase 2 — vendored dependencies, per §4 — **the four thin ones DONE (2026-09-15)**

Take the four thin ones as `cfg` ports onto our own bases. Redo the `tree-sitter`, `lsp-types`,
`which` and `async-tar` decisions from scratch. Do not import the stale snapshots.

`web/vendor/` now holds `agent_client_protocol_patch`, `url_wasm`, `smol_wasm` and
`wasm_thread_patch` — `docs/phase2-vendored.md`, reviewed in `docs/phase2-review-round2.md`. The
content mandate reproduced exactly under adversarial re-measurement: ACP's 50 non-`lib.rs` files
hash-identical to crates.io 2.0.0 with only the four authorised `cfg` lines; `url_wasm` differs from
registry 2.5.7 in `src/lib.rs` alone with **zero** rustfmt noise; `smol_wasm`'s entire native half is
`pub use smol_real::*;`; `wasm_thread_patch`'s manifest byte-identical to git `0cf96c77`. §4.1's wall
claim holds too — the web wasm32 graph contains none of `async-io`, `async-process`, `polling`,
`errno`, `rustix`, `blocking`.

**The defects were all at the integration layer, which `cargo metadata --no-deps` cannot see.** Worth
recording, because the same shape will recur in Phases 3 and 4:

- **One genuine silent regression, exactly the §11 class.** `url_wasm` selected its wasm branches on
  bare `target_arch = "wasm32"`, which is also true on `wasm32-wasip1/p2` and emscripten — targets
  where url 2.5.7 has *real* `from_file_path`/`to_file_path`. The fork replaced working
  implementations with the browser stub. Narrowed to
  `all(target_arch = "wasm32", target_os = "unknown")`.
- The vendored ACP kept two `[[test]]` stanzas pointing at an unpublished path crate that crates.io
  normalisation strips. Registry builds never compile test targets; a **workspace member** does, so
  `cargo check --workspace` failed where the registry crate never would.
- With no `web/Cargo.lock`, the web graph had already drifted to
  `agent-client-protocol-derive` 2.1.0 while the root pins 2.0.0. §11 asks for both locks committed;
  only one existed.
- `[patch.crates-io] wasm_thread` is **inert** — `gpui_web` and `scheduler` take it from the git URL,
  so only the `[patch."https://github.com/zed-industries/wasm_thread"]` table does any work.

Still open per §4: `tree-sitter`, `lsp-types`, `which`, `async-tar`.

### Phase 3 — the 174 `WASM_CFG` files plus manifests

**Not mechanical — Phase 0 measured it** [verified]. `git merge-tree` of `andy/web-version` against
`zedweb/zed-web` produces **32 conflicted files / 70 conflict hunks**, not the handful §5.4 implied,
and several are structural rather than adjacent-edit: the `gpui_web` touch model, `gpui::Window`,
`rpc::peer`, `assets`.

The important part of that number is where it comes from. **28 of the 32 are not our doing**: they
come from `origin/main` having moved 221 commits (702 files) past the `fecc3273` merge-base that
`zed-web` was cut from. Only the remainder traces to our own 29 commits — §5.4's table is right
about our local features being cheap; it simply never counted upstream drift, which is a different
axis. Two consequences:

- The cost grows with time, not with our work. Every week we do not land this, `origin/main` adds
  drift. That argues for doing Phase 3 sooner and rebasing often, and it is the strongest argument
  in this document for adopting `web/sync-upstream.sh` early rather than at the end.
- The §5.3 refusals cannot be handled here. They do not conflict, so a careful merge never surfaces
  them — see §5.3's post-merge audit.

The four real §5.4 collision files still get merged by hand.

#### Landed — 3a (manifests) and 3b (`.rs`), 2026-09-15 ✅

Split by file type so the two could run without clobbering each other: **3a** took every `Cargo.toml`
(`docs/phase3a-manifests.md`, reviewed round 3 in `docs/phase3a-review-round3.md`), **3b** took every
`.rs` (`docs/phase3b-wasm-cfg.md`). Final state: **48 manifests + 72 `.rs` across 43 crates.**

| Check | Result |
| --- | --- |
| Desktop native compile, all 43 changed crates | **`Finished dev profile in 1m 37s`, 0 errors** [verified] |
| `web/check-workspace-isolation.sh` | 31/31 |
| `web/check-refusals.sh` | 4/4, green *throughout* 3b — including while it was editing `terminal.rs`, the §5.3.2 mine |
| Root `Cargo.lock` | 44 insertions, **0 deletions**; no `[[package]]`, version, source or checksum changed |
| Nine pinned crates (§9) | sources byte-identical |

That compile is what retires §5.1's claim that `WASM_CFG` leaves the native path unchanged: it was a
classification, now it is a compiler result on our tree.

**§5.5's planted defect is fixed.** `crates/rpc/src/proto_client.rs` now reads `use
web_time::Instant;`. That one travelled the whole plan: noticed while reviewing this document,
written up as the §5.5 rule, proved real by Phase 0b's isolated probe, and finally corrected here.

**A method note worth keeping.** The first run of the desktop compile reported exit code 0 while
having compiled nothing — a shell quoting fault meant `cargo` received all 43 package names as one
argument, and the 0 came from the tail of the pipeline. Had the exit code been trusted, a false
green would have entered this document and every later phase would have rested on it. **Read the
output, not the status.**

### Phase 4 — `zed_web_server`, `wasm_rpc`, `wasm_remote`, `zed_web_workspace`

Mostly new files, so mostly copy. `zed_web_server` goes in the root workspace.

### Phase 5 — first light

#### The onion, peeled in layers [verified]

`cargo check --workspace --target wasm32-unknown-unknown`, run from `web/`, was the instrument for
all of this. Each run's failures were the next round's input list — no estimate, no inference:

| Layer | Crates | Errors | Cause |
| --- | --- | ---: | --- |
| 1 | `paths`, `settings_json` | 18 | §6.3's HOME hole; Phase 3a gated the manifest but 3b never gated the matching source |
| 2 | `askpass`, `fuzzy`, `lsp`, `rpc` | 19 | `util::shell` / `util::command` / `util::fs` cfg'd out but still used downstream; `zstd` gated in the manifest only; `BackgroundExecutor::scoped` absent on wasm |
| 3 | `async-tar`, `wasmtime` | 27 | **No first-party crate left** — only the two §4 vendoring items Phase 2 had deliberately deferred |

Layer 2 is worth naming as a process defect, not just a bug: splitting Phase 3 into a manifest agent
and a `.rs` agent let a dependency be gated in `Cargo.toml` while its `use` stayed unconditional.
**The seam between two parallel agents is where the work falls through.** Either give one agent both
halves of a change, or add a gate that pairs them.

Layer 3 is the good news: by then `gpui`, `editor`, `project`, `workspace`, `rpc`, `lsp` and `util`
all compiled for `wasm32-unknown-unknown`. The remaining two were `tree-sitter` (whose `wasm` feature
still pulled `wasmtime`) and `async-tar`, both already dispositioned in §4.

#### Rulings on behaviour that could not be preserved

Most of the port is `cfg` gating that leaves the native path byte-identical. A few places could not
be, because the browser genuinely cannot do what the desktop does. Each was escalated rather than
decided by the implementing agent, and each is recorded here so a later reader does not have to
reverse-engineer the intent:

| Site | Native | Web | Ruling |
| --- | --- | --- | --- |
| `AppDatabase::new` (`crates/db/src/db.rs`) | `gpui::block_on` opens the on-disk DB | `panic!` naming `open_in_memory` as the alternative | **Accepted.** Loud failure, not a silent stub. `block_on` cannot exist on the browser main thread. |
| `SettingsStore` init (`crates/settings/src/settings_store.rs`) | blocks for the first settings content, applies it, *then* starts the watcher | skips the blocking read; the same watcher stream delivers that first content asynchronously | **Accepted.** Content is not lost, only delayed by a frame. The visible difference is a brief startup window where defaults apply before user settings land. |
| `util::paths::home_dir` | `dirs::home_dir()` | `/workspace` placeholder | **Temporary.** Phase 6's `Home::` RPC replaces it; until then anything expanding `~` is wrong on web. Marked in-source (§6.3). |

The rule the agents were given, and which produced these: **a wasm stub must fail honestly —
`panic!` with the alternative named, or `Err(ErrorKind::Unsupported)` — never a fabricated success.**
Where an agent believed only a silent success was possible, it was told to stop and report instead of
deciding. That is what surfaced all three of the above.

#### BLOCKER — `tree_sitter::Language` is not `Send`/`Sync` on wasm, and making it so would be unsound

This is the one thing in Phase 5 that a `cfg` cannot fix, and it needs a decision above the
implementation layer. `crates/language` requires `Language: Send + Sync`; on `wasm32` the vendored
bindings do not provide it, producing 10 errors that survive every other layer.

The tempting fix — add `unsafe impl Send/Sync for Language` to `web/vendor/tree_sitter_wasm` — was
investigated and **refused**, correctly [verified]:

- `zed-web`'s snapshot (`7f534862`) **has** those `unsafe impl`s ungated.
- Our base (`43623ec`) **gates them**, and the reason is upstream issue **#5851 — a soundness fix**
  about the threading model, not about wasmtime.
- The wasmtime reason genuinely does not apply to us: §4's decision (b) means `wasm = ["std"]` never
  pulls it, `wasmtime-c-api` is native-only, and `cargo metadata --filter-platform
  wasm32-unknown-unknown` finds **zero** wasmtime packages in our graph.
- **But #5851's reason does apply.** Adding the impls would undo an upstream soundness fix for the
  very threading model the web build ships (wasm atomics + workers).

**This vindicates §4's central instruction in the strongest possible way.** §4 said: do not import the
stale snapshot, redo the decision on `43623ec`. Had we copied `zed-web`'s tree wholesale, we would
have silently inherited an unsound threading model — no compiler error, no failing test, exactly the
class of defect this whole document has been hunting.

Three ways out, none of them vendor-level, all needing a ruling:

| Option | Cost | Note |
| --- | --- | --- |
| Rework `crates/language` so `Language`/`Parser` stay on their creating thread and only `Tree`s cross | Real design work in a core crate | The shape upstream intends |
| Compile the tree-sitter C core for wasm and adopt #5851's `copy_without_callbacks` protocol | Needs the WASI SDK (unauthorised) | Closest to upstream |
| Drop `+atomics` and ship single-threaded wasm | Changes the whole build shape, §2.3's `SharedArrayBuffer` story | Undoes `web/build.sh`'s premise |

Until one is chosen, Phase 5 cannot reach a green `cargo check`, and no amount of further layer
peeling changes that. **The correct next action is a decision, not more porting.**

#### What `cargo check` cannot tell us

Passing `cargo check` is **not** first light. Two heavyweight prerequisites remain, and neither is
this session's to install unilaterally:

1. **A nightly toolchain** — `-Z build-std=std,panic_abort`, required by `+atomics`. `web/build.sh`
   detects its absence and refuses with instructions rather than installing it.
2. **WASI SDK v25** — §4.2's ruling: the web build compiles **18 tree-sitter grammar C libraries**
   for wasm32 because `markdown` enables `load-grammars`. `cargo check` does not run those C builds
   to completion the way `cargo build` does, so a green check understates what the link step needs.

Until both exist, the honest status is "the Rust side type-checks for wasm", not "it builds".

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
- ~~Root `Cargo.lock` must not gain a `web/` member or any `path` source pointing into `web/`.~~
  **This check is unachievable as written, and a grep for it passes while contaminated** [verified,
  round-1 review]. An isolation fixture proved it: a crate at `web/vendor/forked` appears in the root
  lock as a bare `[[package]] name = "forked"` with **no `source` field and no path at all**, even
  with `exclude = ["web"]` set. There is nothing in the lock file to grep for. Replace it with:
  *no package in `cargo metadata`'s root-workspace output may have a `manifest_path` under `web/`* —
  which is what `web/check-workspace-isolation.sh` asserts.

  Related, and worth knowing about `exclude`: it governs **membership, not the dependency graph**
  (removing it does pull `web/vendor/forked` in as a member — also verified). And §3.2's worry that
  `cd web && cargo build` would walk up into the root workspace is already prevented by
  `web/Cargo.toml` carrying its own `[workspace]` table; `exclude` is never reached for that case.
- `cargo tree` for those nine, run from the root, must resolve to the same sources.
- The desktop bundle must still build and install.

### 9.1 The second lock must be **seeded** from the first, not resolved from scratch [verified]

§11 listed "two `Cargo.lock` files drift" as a Medium risk mitigated by "both committed". That
mitigation is wrong: committing a lock records drift, it does not prevent it. The drift arrived
immediately and was large.

`web/Cargo.lock` was created from nothing in Phase 2, so Cargo resolved every dependency to the
newest compatible release while the root lock has been pinned over months. Measured across the two
files: **1225 shared packages, 445 of them at different versions — 36%.** The web build was
compiling different code from the desktop for more than a third of its shared graph.

It surfaced as a baffling symptom rather than as a version complaint. `cargo check --workspace` in
`web/` failed on `merman`:

```
error[E0432]: unresolved import `merman_render::text::VendoredFontMetricsTextMeasurer`
error[E0599]: no associated function or constant named `parity` found for `TextMeasurementPolicy`
```

while the identical crate built fine from the root. Cause: root pinned `merman`, `merman-core` and
`merman-render` all at `0.8.0-alpha.5`; the web lock had `merman` at alpha.5 but its two siblings at
**alpha.6**, which had moved that API. A version-skew bug wearing a missing-import costume.

**The fix, and the rule:** seed the second lock from the first — `cp Cargo.lock web/Cargo.lock`, then
let `cargo metadata` in `web/` reconcile. Cargo keeps every pin it can and only re-resolves what the
web workspace genuinely changes (the vendored `[patch]` targets). Result: **445 → 0 disagreements**
across 1589 shared packages, `merman` builds, and `url` still resolves to `web/vendor/url_wasm`.

Do the same after every upstream sync, and assert it: *no package name shared by both lock files may
resolve to different versions.* §9's existing checks all guard the desktop from the web; this is the
first one guarding the web from drifting away from the desktop, which is the direction that actually
bit.

Two standing rules, both learned the hard way in this repo:

- Never run `./script/clippy` or any other `--release --all-features` cargo command between
  `script/bundle-mac` runs. It alternates configurations in the shared `target/release` and has
  previously triggered a ~25 minute webrtc re-download. Use `cargo clippy -p <crate> --lib --tests`.
- The web build must never share `target/` with the desktop build. Different rustflags, different
  patches, different toolchains.

## 10. Open questions

None of these block starting, but each will need an answer before the phase that depends on it.

1. **Nothing has been compiled for wasm.** The expectation that the four crates fail to build
   inside `zed_web_workspace` today is an inference, not a compiler message. Promoted to the first
   row of §11 and given a phase of its own (Phase 0b), because it is not a question that sits
   politely beside the others: it is the one that decides whether the rest of this document's
   estimates mean anything.
2. ~~Which `cfg` branch `process_start_times` selects on `wasm32-unknown-unknown`.~~
   **ANSWERED by Phase 0b, and it is the bad answer.** `rustc --print cfg --target
   wasm32-unknown-unknown` emits no `unix` at all, so of the three arms in
   `crates/remote/src/claude_sessions.rs` (`:205` linux, `:236` unix-not-linux, `:278` `not(unix)`)
   wasm32 takes `not(unix)` — the empty map. Run in a browser, Claude session liveness marks
   **every session dead, silently**. This is now a design constraint on feature 3, not a question:
   liveness must execute server-side, and §6.4 already says so. Verified with a `compile_error!`
   probe on the same three predicates.
3. ~~Whether `url` 2.5.8 already carries the wasm file-path support, which would remove one fork.~~
   **ANSWERED by Phase 2: no.** 2.5.8 still gates `from_file_path`/`to_file_path` on a target list
   that does not include `target_os = "unknown"`, so the methods do not exist on
   `wasm32-unknown-unknown`. The fork stays, cut from 2.5.7 (the version in our lockfile) with only
   the wasm branches ported.
4. Whether `which` 8's `Sys` trait can replace the 6.0.3 stub outright.
5. ~~Whether `tree-sitter` git `43623ec` can build for `wasm32-unknown-unknown` at all.~~
   **ANSWERED, with an important qualifier.** Phase 0b: its C build fails for wasm32 at
   `lib/src/wasm_store.c:315` (implicit `printf`, no libc headers) — **but that was with the host
   clang**, which is not what `zed-web` uses. `web/build.sh:47-51` downloads a WASI SDK and exports
   `CC_wasm32_unknown_unknown` plus `CFLAGS_wasm32_unknown_unknown=-isystem
   <wasi-sysroot>/include/wasm32-wasi` before building. So the honest answer is: **not as-is, and
   not with the host toolchain.** How much of the wall that one export removes — `zstd-sys` and the
   `tree-sitter-*` grammars all failed the same way — is the question `docs/phase2-dependency-wall.md`
   is measuring. Do not treat "tree-sitter cannot build for wasm" as settled; treat "we need a WASI
   SDK C toolchain, and §4's disposition has to be redone on top of that" as the finding.
6. Whether `TMUX` and other environment variables survive `process_rpc`, which forwards only the
   env the client supplies.
7. Whether the four panels have native-only dependencies in their UI layers (icons, clipboard,
   notifications) that have not been surveyed.
8. What a `project_manager` "local path" means to a user in a browser: these are server paths, and
   there is no folder picker wired to that panel.
9. **Who owns the tooling that `exclude = ["web"]` now skips.** Raised by the round-1 review: web
   crates fall outside `xtask package-conformity` and `script/generate-licenses`. Neither blocks
   development, both block shipping. Decide before Phase 5 whether those tools learn about the second
   workspace or whether `web/` gets its own invocation.
10. **Where the wasm build entrypoint lives, given F3 and F4.** `web/.cargo/config.toml` is
    discovered from the *working directory*, so every wasm build must run with `web/` as its cwd or
    it silently gets the desktop flags; and `+atomics` needs `-Z build-std` on nightly, which the
    repo's stable `rust-toolchain.toml` does not provide. Both are recorded in the config's header
    comment and asserted by the gate, but the actual fix is a `web/build.sh` port in Phase 4. Until
    that exists, **the skeleton cannot produce a wasm binary** — by design, not by oversight.

## 11. Risk register

| Risk | Severity | Mitigation |
| --- | --- | --- |
| **Nothing in this plan has been compiled for wasm.** Every porting estimate in §5 and §6 is read off source, not reported by a compiler, so the schedule has no floor under it. §5.5 is the proof that this class of defect is real and invisible on native: `proto_client.rs` mixes `std::time::Instant` with `web_time::Instant`, which is the same type off wasm and a type error on it. How many more exist is unknown. | High | Phase 0b: `cargo check --target wasm32-unknown-unknown` against `rpc` and `remote` before any porting, for an error count and taxonomy. Treat §6's effort column as unvalidated until that output exists. |
| ~~**§3.2's load-bearing assumption is unverified.**~~ **RESOLVED — the assumption holds** [verified 2026-09-15, brain]. A throwaway member under `web/` path-depping `crates/context_server` (a root crate that depends on `url`) was added, and `cd web && cargo metadata --filter-platform wasm32-unknown-unknown` resolved a 539-package graph in which `url` → `web/vendor/url_wasm`, `smol` → `web/vendor/smol_wasm`, `agent-client-protocol` → `web/vendor/agent_client_protocol_patch`, `wasm_thread` → `web/vendor/wasm_thread_patch`. Root crates reached by `path` **do** build against the web workspace's `[patch]` graph, so the vendored forks apply and §3.2 is load-bearing rather than decorative. Probe removed; gate back to 28/28; root `Cargo.lock` byte-identical throughout. | — | Closed. Phase 4 may write its 97 dependency keys against this. |
| **The §5.3 refusals land silently.** Phase 0 measured it: three of the four do not conflict, and the fourth's refused line auto-merges even though its file conflicts elsewhere. A refusal list only protects you if the merge stops on it, and this one does not. | High | §5.3's post-merge audit — four exact assertions against the merged tree, scripted, re-run after every `sync-upstream`, not a human reading a diff |
| **The dependency wall is wider than §4's nine forks.** Phase 0b: `getrandom` 0.2 and 0.3, `errno`, `polling`, `zstd-sys`, `tree-sitter-json`, `trash` and `wasmtime` also block wasm32, and `tree-sitter@43623ec` does not build for it at all. Phase 2 is scoped against a list known to be incomplete. | High | Measure the real list before committing to Phase 2's shape; `docs/phase2-dependency-wall.md` maps how `zed-web` neutralises each one |
| **Merge cost grows with calendar time, not with our work.** 28 of the 32 conflicts come from `origin/main` drifting 221 commits past `zed-web`'s merge-base. | Medium | Land Phase 3 sooner rather than later; adopt `web/sync-upstream.sh` early, not at the end |
| A vendored fork silently rolls a dependency backwards | High | §4 disposition table; §9 lock-file check after every phase |
| `RELEASE_CHANNEL` change leaks into the desktop build | High | §5.3 refusal list; `script/bundle-mac` already demonstrated this failure mode |
| Rebase cost is larger than the diff suggests | Medium | Phase 0 measures it before any work starts |
| `forward_ports` semantics never settle | Medium | Scoped last; ship detection-and-URL before any tunnelling |
| ~~Two `Cargo.lock` files drift~~ **This happened, at 36%** [verified] — see §9.1 | **was Medium, is High** | Committing both records drift, it does not prevent it. `web/Cargo.lock` must be **seeded from the root lock**, and a gate must assert zero version disagreement on shared packages |
| Upstream rebases become expensive | Medium | `web/sync-upstream.sh` is adopted along with the code |
| iPad is unusable | Low | Explicitly not a requirement; MacBook is the target |
