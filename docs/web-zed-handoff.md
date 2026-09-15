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
| **5** | **in progress** — 28 crates compile for wasm32; `./web/build.sh` reaches `-Z build-std` and the WASI grammar C compile, then fails on whichever crate is next |
| 6 | not started |

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
cd web && CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=../target/web-probe \
    cargo check -p <crate> --target wasm32-unknown-unknown --lib
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=target/web-probe cargo check -p <crate> --lib
```

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
| `web/check-wasm-time.sh` + `wasm-std-instant.allowlist` | §5.5's `Instant` rule; needs `zed_web_workspace` so it cannot run before Phase 4 |

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
