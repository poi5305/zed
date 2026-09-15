# Web Zed — Handoff

Where the port stands, what is proven, and what the next session needs to know that the
commit messages and `docs/web-zed-plan.md` do not already say.

- **Branch:** `andy/web-version`
- **Plan:** `docs/web-zed-plan.md` — kept current; six of its claims were overturned by
  execution and rewritten in place, each marked `[verified]`
- **Reports:** `docs/phase*.md` — 28 measurement and review reports, one per work unit

## 1. Status

The plan defines eight phases -- 0, 0b, 1, 2, 3, 4, 5, 6. Sections 7 to 11 of the document
are sections, not phases.

| Phase | State |
| --- | --- |
| 0 · 0b | ✅ measurement complete |
| 1 · 2 · 3 | ✅ |
| 4 | ✅ **now genuinely** — it was recorded as done while `zed_web_workspace` referred to twelve things that did not exist; see §1b |
| **5 · first light** | ✅ **`./web/build.sh` exits 0 and writes a 72 MB module to `web/dist/static/`** |
| **6** | `Home::` RPC done; three panels registered and routed; `forward_ports` dispositioned by §6.5. Two §6.4 items remain, both listed in §5 below |

Desktop is unaffected and asserted, not assumed: both lock files have gained lines and deleted
none, every touched crate passes `cargo check` natively, and `cargo fmt --all -- --check` is
clean apart from two files that predate this work.

## 1b. Phase 4 was recorded as complete and was not

An entry-point crate is the last thing a workspace check reaches, because nothing depends on
it. Every error in front of it hides every error inside it, so `zed_web_workspace` could be
written, reviewed and signed off while referring to an API surface that was only ever planned.
Twelve defects of that one shape were found and fixed once it finally compiled:

| | |
| --- | --- |
| `extensions_ui::init_remote_store` | called once, defined nowhere |
| `web_extensions.rs` | 512 lines, never declared as a module, never compiled |
| nine APIs | `sqlez::remote_sql`, `db::prepare_web_database`, `assets::install_web_assets`, `terminal::set_remote_client`, two `settings` keymap paths, `Session::for_web`, `Workspace::initial_state_loaded`, `PlatformTitleBar::set_left_padding` |
| `smol_wasm/src/rpc.rs` | a stand-in whose own header said Phase 4 would replace it. Its `call` returned "not wired yet", so **every `Fs::*` and `Process::*` call in the browser failed** |
| `ActivityIndicator::new` | called with four arguments; it has taken three since before this port |
| `SettingsWindow` | missing `Focusable`, `EventEmitter`, `new_modal`, `open_page` |

**The rule that follows:** a crate nothing depends on needs its own `cargo check` from the day
it is created, even -- especially -- when it cannot yet link.

## 2b. Every remaining layer has had one of three shapes

Worth knowing before opening the next error list, because it tells you which question to ask
first:

1. **A dependency gated out of the manifest whose `use` stayed unconditional.** Seen in
   `sidebar` (`recent_projects`), `settings_ui` (five deps), `keymap_editor` (two grammars).
   The plan names this as Phase 3's process defect — a manifest agent and a `.rs` agent with a
   seam between them — and it is still the single commonest failure.
2. **A gate that was over-conservative and can simply be removed.** Ask this *before* deleting
   UI: `codestral`, `edit_prediction` and `edit_prediction_ui` were gated out of `settings_ui`
   in Phase 3a and all three build for wasm today, so three settings pages were recovered by
   deleting five lines of manifest rather than by cfg'ing out features.
3. **A `std::time::Instant` the §5.5 sweep missed**, which compiles and then panics in a
   browser. `web/check-wasm-time.sh` now finds these — see §5.6 of the plan for why it could
   not before.

**Measure shape 2 with a compile, not with a guess.** A probe that greps the output for
`^error` reports success when cargo *panicked* before compiling anything, which happened here
and produced a wrong ruling that had to be withdrawn. Check the exit code, or compile the
dependent crate and read what actually changed.

## 3. Rulings that must not be quietly undone

These are the decisions a later reader is most likely to "clean up" without realising what
they cost. Each is load-bearing.

### `tree_sitter::Language` stays `!Send`/`!Sync`

Our base `43623ec` gates `unsafe impl Send/Sync for Language` behind upstream's **#5851
soundness fix**. `zed-web` carries those impls only because it forked the older
`7f534862`, which predates it. Adding them back makes `cargo check` green in one line and
**undoes a soundness fix for the threading model the web build actually ships** (wasm
atomics + workers), with nothing in any test suite to notice.

Instead, the three sites that shipped a `Language` across threads take the foreground
executor on wasm. `crates/project`'s 14 `Pin<Box<dyn Future + Send>>` errors were the bill
for that decision — finite, concrete, and paid.

### `MaybeSend` / `MaybeSync` are the bill for that, and they are cheap

The `!Send` ruling above propagates up through everything that holds a `Language`, and the
bounds it collides with are written in three different places. Each needs its own shape:

| Where the bound is written | How wasm relaxes it |
| --- | --- |
| A trait's method signature (`-> impl Send + Future<…>`) | `MaybeSend`, a marker trait: `Send` off wasm via a blanket `impl<T: Send>`, empty on wasm. 28 sites. |
| A trait's supertrait list (`LspAdapter: Send + Sync`) | `MaybeSend + MaybeSync`, same shape. |
| A trait **object** (`dyn Fn() -> … + Send + Sync`) | a cfg'd type alias, because only auto traits may follow the principal trait in a trait object — `dyn Fn() + MaybeSend` does not compile. |

Native behaviour is unchanged by construction: `MaybeSend: Send` with a blanket impl for every
`T: Send` means the native bound is still exactly `Send`. That is what makes these safe to
add and dangerous to "simplify" — deleting them reads like tidying and silently re-imposes a
bound wasm cannot satisfy.

### `fuzzy::match_strings` matches inline on wasm

`match_strings` fans out through `executor.scoped(...)`, which only completes once its
spawned tasks are polled — impossible while a caller synchronously awaits it on a
single-threaded dispatcher. So `now_or_never()` at the six `agent_ui` call sites would have
compiled and left **every fuzzy search in the browser permanently empty**.

The fix is in `crates/fuzzy`: a wasm path that matches inline (same results, no
parallelism, which is what one thread gives anyway), plus `match_strings_blocking` whose
`unreachable!` documents and enforces "the wasm path cannot suspend". That one change
serves all **23** callers of `match_strings`, not just `agent_ui`'s six.

### Every wasm stub fails honestly

The rule given to every agent, and the reason three behaviour differences surfaced for a
ruling instead of being silently decided:

> A wasm stub must fail honestly — `panic!` naming the alternative, or
> `Err(ErrorKind::Unsupported)` — never a fabricated success. If you believe only a silent
> success is possible, stop and report instead of deciding.

Worked examples, each with its reasoning in-source: `AppDatabase::new` panics naming
`open_in_memory`; `SettingsStore` lets the existing watcher deliver the first settings a
frame later rather than dropping them; `worktree::insert_entry` and `wrap_map` assert with
`now_or_never()` **and panic**, because skipping would lose data or paint unwrapped text;
`command_palette` uses `try_recv()` **without** panicking, because "not ready yet" is an
outcome its caller already handles.

### `web/check-refusals.sh` is a post-merge audit, not a merge filter

§5.3 lists four upstream changes that must never arrive. Phase 0 measured that **three of
them do not conflict** — a careful merge never surfaces them. Run the script after every
merge and every `sync-upstream`, not once.

### The second lock is seeded, never resolved

445 of 1225 shared packages had drifted between the two lock files, surfacing as `merman`
failing to build with an unresolved import while the identical crate built fine from the
root. Re-seed with `cp Cargo.lock web/Cargo.lock` and let `cargo metadata` in `web/`
reconcile. `web/check-workspace-isolation.sh`'s **L1** asserts it, using subset semantics:
web may use fewer versions than root, never a version root has not vetted.

## 4. Gates

Run all three before believing anything:

| Script | Guards |
| --- | --- |
| `web/check-workspace-isolation.sh` | §9's desktop-isolation invariants, plus the §3.2 rules nothing else enforced. It has already caught a real regression — another agent rewrote `web/Cargo.toml` wholesale half an hour after the assertions landed, dropping `[profile.web-release]` and four `[patch]` entries |
| `web/check-refusals.sh` | the four §5.3 refusals, bound to behavioural strings rather than line numbers |
| `web/check-wasm-time.sh` + `wasm-std-instant.allowlist` | §5.5's `Instant` rule. **It had never actually run** — it resolved `-p zed_web_workspace` against the *root* manifest, which excludes `web/`, so every invocation died on `package ID specification did not match any packages`. Fixed to use `web/Cargo.toml`; it went red immediately and found five live `Instant::now()` sites in the wasm graph |

### What the gate reports today, and which half of it is real

`check-workspace-isolation.sh` last recorded **0 failures** at review round 4. It now reports
four, and they are not one thing:

| Report | Verdict |
| --- | --- |
| `V11` × 2 (`cd web && cargo check --workspace` fails) | **expected.** Making that command succeed *is* Phase 5. It goes green when Phase 5 finishes, and not before. |
| `M1` — `zed_web_workspace` declares `gpui_web`, `wasm_rpc`, `feature_flags`, `edit_prediction`, `instant`, `agent-client-protocol`, none of which its own `src/` ever names | **a real finding, verified by hand.** Either they are load-bearing for a reason no comment records, or they are dead manifest lines that reached `Cargo.lock`. Phase 4's author is the one who knows; resolve it before Phase 6. |
| `M1`/`M2` — every `web/vendor/*` fork | **a scope defect in the gate.** `phase3a_base` is a fixed commit, so the diff has grown to cover Phases 4 and 5 as they landed. These manifests are upstream's, kept verbatim on purpose, and `M1` cannot see their sources anyway (`tree_sitter_wasm` builds from `binding_rust/`, not `src/`). Exclude `web/vendor/` from M1 and M2. |

Fixing the third is what keeps the second readable. A gate that cries wolf about legitimate
work is one nobody reads, and this one has already caught a real regression once.

## 5. What is left

**Nothing has been opened in a browser yet.** The build produces a module; that it runs is an
assumption. Serving `web/dist/` and opening a project is the next action, and it will find
things this session could not.

**Phase 6's code is done, including the verification §6.4 gated it on.** The tmux quoting and
`;` handling were tried against tmux 3.6b, reproducing the exact argv `send_text_arguments`
builds — passed as separate arguments, not through a shell, because that is the difference that
would have hidden the bug:

| | |
| --- | --- |
| `";"` as its own argument | tmux reads it as a command separator; all three commands ran |
| a `;` *inside* the text | survived — it is buffer content, never a command-line argument |
| `# Web Zed — Handoff

Where the port stands, what is proven, and what the next session needs to know that the
commit messages and `docs/web-zed-plan.md` do not already say.

- **Branch:** `andy/web-version`
- **Plan:** `docs/web-zed-plan.md` — kept current; six of its claims were overturned by
  execution and rewritten in place, each marked `[verified]`
- **Reports:** `docs/phase*.md` — 28 measurement and review reports, one per work unit

## 1. Status

The plan defines eight phases -- 0, 0b, 1, 2, 3, 4, 5, 6. Sections 7 to 11 of the document
are sections, not phases.

| Phase | State |
| --- | --- |
| 0 · 0b | ✅ measurement complete |
| 1 · 2 · 3 | ✅ |
| 4 | ✅ **now genuinely** — it was recorded as done while `zed_web_workspace` referred to twelve things that did not exist; see §1b |
| **5 · first light** | ✅ **`./web/build.sh` exits 0 and writes a 72 MB module to `web/dist/static/`** |
| **6** | `Home::` RPC done; three panels registered and routed; `forward_ports` dispositioned by §6.5. Two §6.4 items remain, both listed in §5 below |

Desktop is unaffected and asserted, not assumed: both lock files have gained lines and deleted
none, every touched crate passes `cargo check` natively, and `cargo fmt --all -- --check` is
clean apart from two files that predate this work.

## 1b. Phase 4 was recorded as complete and was not

An entry-point crate is the last thing a workspace check reaches, because nothing depends on
it. Every error in front of it hides every error inside it, so `zed_web_workspace` could be
written, reviewed and signed off while referring to an API surface that was only ever planned.
Twelve defects of that one shape were found and fixed once it finally compiled:

| | |
| --- | --- |
| `extensions_ui::init_remote_store` | called once, defined nowhere |
| `web_extensions.rs` | 512 lines, never declared as a module, never compiled |
| nine APIs | `sqlez::remote_sql`, `db::prepare_web_database`, `assets::install_web_assets`, `terminal::set_remote_client`, two `settings` keymap paths, `Session::for_web`, `Workspace::initial_state_loaded`, `PlatformTitleBar::set_left_padding` |
| `smol_wasm/src/rpc.rs` | a stand-in whose own header said Phase 4 would replace it. Its `call` returned "not wired yet", so **every `Fs::*` and `Process::*` call in the browser failed** |
| `ActivityIndicator::new` | called with four arguments; it has taken three since before this port |
| `SettingsWindow` | missing `Focusable`, `EventEmitter`, `new_modal`, `open_page` |

**The rule that follows:** a crate nothing depends on needs its own `cargo check` from the day
it is created, even -- especially -- when it cannot yet link.

## 2b. Every remaining layer has had one of three shapes

Worth knowing before opening the next error list, because it tells you which question to ask
first:

1. **A dependency gated out of the manifest whose `use` stayed unconditional.** Seen in
   `sidebar` (`recent_projects`), `settings_ui` (five deps), `keymap_editor` (two grammars).
   The plan names this as Phase 3's process defect — a manifest agent and a `.rs` agent with a
   seam between them — and it is still the single commonest failure.
2. **A gate that was over-conservative and can simply be removed.** Ask this *before* deleting
   UI: `codestral`, `edit_prediction` and `edit_prediction_ui` were gated out of `settings_ui`
   in Phase 3a and all three build for wasm today, so three settings pages were recovered by
   deleting five lines of manifest rather than by cfg'ing out features.
3. **A `std::time::Instant` the §5.5 sweep missed**, which compiles and then panics in a
   browser. `web/check-wasm-time.sh` now finds these — see §5.6 of the plan for why it could
   not before.

**Measure shape 2 with a compile, not with a guess.** A probe that greps the output for
`^error` reports success when cargo *panicked* before compiling anything, which happened here
and produced a wrong ruling that had to be withdrawn. Check the exit code, or compile the
dependent crate and read what actually changed.

## 3. Rulings that must not be quietly undone

These are the decisions a later reader is most likely to "clean up" without realising what
they cost. Each is load-bearing.

### `tree_sitter::Language` stays `!Send`/`!Sync`

Our base `43623ec` gates `unsafe impl Send/Sync for Language` behind upstream's **#5851
soundness fix**. `zed-web` carries those impls only because it forked the older
`7f534862`, which predates it. Adding them back makes `cargo check` green in one line and
**undoes a soundness fix for the threading model the web build actually ships** (wasm
atomics + workers), with nothing in any test suite to notice.

Instead, the three sites that shipped a `Language` across threads take the foreground
executor on wasm. `crates/project`'s 14 `Pin<Box<dyn Future + Send>>` errors were the bill
for that decision — finite, concrete, and paid.

### `MaybeSend` / `MaybeSync` are the bill for that, and they are cheap

The `!Send` ruling above propagates up through everything that holds a `Language`, and the
bounds it collides with are written in three different places. Each needs its own shape:

| Where the bound is written | How wasm relaxes it |
| --- | --- |
| A trait's method signature (`-> impl Send + Future<…>`) | `MaybeSend`, a marker trait: `Send` off wasm via a blanket `impl<T: Send>`, empty on wasm. 28 sites. |
| A trait's supertrait list (`LspAdapter: Send + Sync`) | `MaybeSend + MaybeSync`, same shape. |
| A trait **object** (`dyn Fn() -> … + Send + Sync`) | a cfg'd type alias, because only auto traits may follow the principal trait in a trait object — `dyn Fn() + MaybeSend` does not compile. |

Native behaviour is unchanged by construction: `MaybeSend: Send` with a blanket impl for every
`T: Send` means the native bound is still exactly `Send`. That is what makes these safe to
add and dangerous to "simplify" — deleting them reads like tidying and silently re-imposes a
bound wasm cannot satisfy.

### `fuzzy::match_strings` matches inline on wasm

`match_strings` fans out through `executor.scoped(...)`, which only completes once its
spawned tasks are polled — impossible while a caller synchronously awaits it on a
single-threaded dispatcher. So `now_or_never()` at the six `agent_ui` call sites would have
compiled and left **every fuzzy search in the browser permanently empty**.

The fix is in `crates/fuzzy`: a wasm path that matches inline (same results, no
parallelism, which is what one thread gives anyway), plus `match_strings_blocking` whose
`unreachable!` documents and enforces "the wasm path cannot suspend". That one change
serves all **23** callers of `match_strings`, not just `agent_ui`'s six.

### Every wasm stub fails honestly

The rule given to every agent, and the reason three behaviour differences surfaced for a
ruling instead of being silently decided:

> A wasm stub must fail honestly — `panic!` naming the alternative, or
> `Err(ErrorKind::Unsupported)` — never a fabricated success. If you believe only a silent
> success is possible, stop and report instead of deciding.

Worked examples, each with its reasoning in-source: `AppDatabase::new` panics naming
`open_in_memory`; `SettingsStore` lets the existing watcher deliver the first settings a
frame later rather than dropping them; `worktree::insert_entry` and `wrap_map` assert with
`now_or_never()` **and panic**, because skipping would lose data or paint unwrapped text;
`command_palette` uses `try_recv()` **without** panicking, because "not ready yet" is an
outcome its caller already handles.

### `web/check-refusals.sh` is a post-merge audit, not a merge filter

§5.3 lists four upstream changes that must never arrive. Phase 0 measured that **three of
them do not conflict** — a careful merge never surfaces them. Run the script after every
merge and every `sync-upstream`, not once.

### The second lock is seeded, never resolved

445 of 1225 shared packages had drifted between the two lock files, surfacing as `merman`
failing to build with an unresolved import while the identical crate built fine from the
root. Re-seed with `cp Cargo.lock web/Cargo.lock` and let `cargo metadata` in `web/`
reconcile. `web/check-workspace-isolation.sh`'s **L1** asserts it, using subset semantics:
web may use fewer versions than root, never a version root has not vetted.

## 4. Gates

Run all three before believing anything:

| Script | Guards |
| --- | --- |
| `web/check-workspace-isolation.sh` | §9's desktop-isolation invariants, plus the §3.2 rules nothing else enforced. It has already caught a real regression — another agent rewrote `web/Cargo.toml` wholesale half an hour after the assertions landed, dropping `[profile.web-release]` and four `[patch]` entries |
| `web/check-refusals.sh` | the four §5.3 refusals, bound to behavioural strings rather than line numbers |
| `web/check-wasm-time.sh` + `wasm-std-instant.allowlist` | §5.5's `Instant` rule. **It had never actually run** — it resolved `-p zed_web_workspace` against the *root* manifest, which excludes `web/`, so every invocation died on `package ID specification did not match any packages`. Fixed to use `web/Cargo.toml`; it went red immediately and found five live `Instant::now()` sites in the wasm graph |

### What the gate reports today, and which half of it is real

`check-workspace-isolation.sh` last recorded **0 failures** at review round 4. It now reports
four, and they are not one thing:

| Report | Verdict |
| --- | --- |
| `V11` × 2 (`cd web && cargo check --workspace` fails) | **expected.** Making that command succeed *is* Phase 5. It goes green when Phase 5 finishes, and not before. |
| `M1` — `zed_web_workspace` declares `gpui_web`, `wasm_rpc`, `feature_flags`, `edit_prediction`, `instant`, `agent-client-protocol`, none of which its own `src/` ever names | **a real finding, verified by hand.** Either they are load-bearing for a reason no comment records, or they are dead manifest lines that reached `Cargo.lock`. Phase 4's author is the one who knows; resolve it before Phase 6. |
| `M1`/`M2` — every `web/vendor/*` fork | **a scope defect in the gate.** `phase3a_base` is a fixed commit, so the diff has grown to cover Phases 4 and 5 as they landed. These manifests are upstream's, kept verbatim on purpose, and `M1` cannot see their sources anyway (`tree_sitter_wasm` builds from `binding_rust/`, not `src/`). Exclude `web/vendor/` from M1 and M2. |

Fixing the third is what keeps the second readable. A gate that cries wolf about legitimate
work is one nobody reads, and this one has already caught a real regression once.

## 5. What is left

**Nothing has been opened in a browser yet.** The build produces a module; that it runs is an
assumption. Serving `web/dist/` and opening a project is the next action, and it will find
things this session could not.

, backtick, single and double quotes, backslash | all survived verbatim |
| a three-line message | arrived as one paste, not three submits |

What protects them is the design the source already documents: the text reaches tmux through
stdin into a paste buffer, so it "never appears on a command line". The test confirms that is
what does the work.

**Also outstanding, all recorded where they were found:**

- **Syntax highlighting is one grammar, not eighteen.** `load-grammars` is off, and no longer
  for an architectural reason: the C now compiles against tree-sitter's own vendored headers
  (§4.4). What blocks the other seventeen is `tree-sitter-bash` 0.25.1 and `tree-sitter-c`
  0.24.2 hitting `tree-sitter-language`'s deliberate "upgrade tree-sitter to 0.27" `#error`,
  and some scanners calling libc without including it. Both are version bumps.
- **`ZED_WEB_RESTRICT_PATHS` is inconsistent.** `Home::dirs` refuses when home falls outside the
  workspace; the `ClaudeSessions::` RPCs read `~/.claude` regardless, as the SSH server does.
  Which is right depends on what the restriction is meant to bound. Escalated, not decided.
- **Four `self.write` closures still fail on wasm**: `get_or_create_remote_connection` (dead
  code on web), `toolchains` (degraded to empty, logged), `set_toolchain`, and
  `save_trusted_worktrees` — which now refuses *before* clearing, because clearing reaches the
  server and succeeds while the re-insert cannot.
- **`zed_web_server` now links `remote`**, and through it `gpui`. It builds on macOS; whether
  the Linux/Docker image still builds and how much that adds is unmeasured.
- **`check-workspace-isolation.sh`'s M1/M2 over-report.** `phase3a_base` is a fixed commit, so
  the diff has grown to cover Phases 4-6. Exclude `web/vendor/` from both. M1's finding about
  `zed_web_workspace` declaring unused dependencies was real and is now partly resolved.
- **`cargo fmt` is dirty on two pre-existing files** (`remote_server/server.rs`,
  `tmux_sessions_panel.rs`), traceable to `bc35645130`. A CI gate.

## 6. A method note

**Read the output, not the status.** This has now failed eight times across the port, always
the same way: a number that looked relevant was taken as the answer.

| What was read | What it meant |
| --- | --- |
| a pipeline's exit code | the exit code of `tail`, not of `cargo` |
| `grep -c '^error'` returning 0 | cargo had *panicked* before compiling anything |
| `df` showing 336 GiB free | the wrong volume; `target/` is on another mount that was 100% full |
| `exit=101, errors=1` | "that package is not in this graph", not "it fails to compile" |
| a check printing `0 errors` | the script exited before running cargo, three times running |
| an `undefined symbol` at link time | `cargo check` never links, so the C had never been compiled |

The last one is the general case: **a green `cargo check` understates what the link step
needs**, which the plan said in advance and which three separate link failures then proved.

Two habits that worked, and are cheap:

1. **Make the success marker explicit, and look for it.** The check script prints `GUARD OK` as
   its last line. Three runs lacked it and still printed `0 errors`; the absence was the signal,
   and it was there to be seen before the false claim reached a commit message.
2. **Ask a delegated agent what the parts it *did not* touch now do.** That question surfaced
   `save_trusted_worktrees` wiping a user's trust state on the web — a data-loss path nobody
   had asked about, in a function nobody had asked it to change.

And one that keeps recurring in the code itself: **the same fact written in two places will
drift, and nothing will notice.** A hard-coded wasm-bindgen version against the lock; a
manifest gate against its `use`; a build script's feature branch against the feature it was
renamed from; a check script's CFLAGS against `build.sh`'s. Every one of those cost a debugging
session. Derive the second copy, or delete it.
