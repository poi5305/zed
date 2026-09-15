# Web Zed — Handoff

Where the port stands, what is proven, and what the next session needs to know that the
commit messages and `docs/web-zed-plan.md` do not already say.

- **Branch:** `andy/web-version`
- **Plan:** `docs/web-zed-plan.md` — kept current; six of its claims were overturned by
  execution and rewritten in place, each marked `[verified]`
- **Reports:** `docs/phase*.md` — 28 measurement and review reports, one per work unit

## 1. Status

| Phase | State |
| --- | --- |
| 0 · 0b | ✅ measurement complete (`phase0-rebase-cost.md`, `phase0b-wasm-report.md`) |
| 1 | ✅ second workspace + review round 1 |
| 2 | ✅ seven vendored forks + review round 2 |
| 3 | ✅ 48 manifests + 72 `.rs` + review rounds 3 and 4 |
| 4a · 4b · 4c | ✅ `build.sh`, three web crates, `zed_web_server` |
| **5** | **in progress** — `cd web && cargo check --workspace --target wasm32-unknown-unknown` is down to `settings_ui` and `keymap_editor`. Everything else in the graph type-checks, including `language`, `languages`, `edit_prediction_ui` and `sidebar` |
| 6 | `Home::` RPC done (server + the wasm seeding API); the four panels not started |

**Desktop is unaffected and that is asserted, not assumed.** Root `Cargo.lock` has gained
lines and deleted none across the whole port; the nine crates §9 pins keep their exact
sources; `cargo check` of every touched crate passes natively.

## 2. How to resume Phase 5

The loop is mechanical and each iteration is one crate:

```
CARGO_BUILD_JOBS=2 ./web/build.sh > /tmp/b.log 2>&1
grep -oE "could not compile [^ ]+" /tmp/b.log | sort -u     # what to fix
grep -E "^error" -A6 /tmp/b.log | head -40                  # why
```

Then fix, and verify **both** targets before moving on:

```
export CC_wasm32_unknown_unknown="$PWD/target/wasi-sdk/bin/clang"
export CFLAGS_wasm32_unknown_unknown="-isystem $PWD/target/wasi-sdk/share/wasi-sysroot/include/wasm32-wasi"

cd web && CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=../target/web-probe \
    cargo check -p <crate> --target wasm32-unknown-unknown --lib
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=target/web-probe cargo check -p <crate> --lib
```

**Export those two variables first.** `./web/build.sh` sets them itself, so a single-crate
`cargo check` run by hand is the only place they go missing — and when they do, the 18
tree-sitter grammars are handed to the host clang, which cannot target wasm32. The failure
names the grammar, not the missing toolchain, so it reads as a broken crate. It made
`debugger_ui` look broken once when it was already fine.

### The environment constraint that shapes everything

**Five subagent runs were killed by the OS for memory pressure.** Not one was a quality
failure — the `language_models` agent had finished all 50 errors before it was killed, and
the previous session wrongly recorded it as unfinished because it never got to run the
check. The cause is simply that this workspace's wasm compile peaks above the machine's
free memory, and any agent must run cargo to verify.

What did not help: lowering `CARGO_BUILD_JOBS` from 4 to 2, restricting agents to single
crates, forbidding `./web/build.sh` in agent prompts. What worked: doing the remaining
edits directly and running one small `cargo check` at a time.

**If the next session has more memory headroom, delegation is fine and faster.** If not,
expect to do it inline.

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

**Phase 5:** keep running the loop in §2 until `./web/build.sh` produces a `.wasm` in
`web/dist/static/`. Every layer so far has been one of a handful of shapes: a dependency
gated in the manifest whose call sites never followed (seen nine times), a `block_on` the
browser cannot perform, or a `std::time::Instant` the §5.5 sweep missed.

**Phase 6:** not started. Note §6.5's finding before planning it — `forward_ports` has no
meaning in a browser, and §6.2's count stands: `crates/remote/src/claude_sessions.rs` has
113 `std::fs` sites.

**Also outstanding:** `cargo fmt --all -- --check` fails on five files that were already
dirty before this work began (`claude_sessions_panel.rs`, `session_store.rs`,
`remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`). Not
caused by the port, not fixed by it, and a CI gate.

## 6. A method note

Four times this session a conclusion was drawn from a process's state rather than its
output, and three of those were caught before they reached the user. The fourth reached a
commit message. The shape is always the same: a pipeline's exit code belongs to its last
command, a killed agent has not necessarily failed, and a gate that has never been red has
not been shown to work.

**Read the output, not the status.** Every significant finding in this port came from
doing that.
