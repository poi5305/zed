# Phase 0b — wasm32 compiler probe of `rpc` and `remote`

Measurement only. No `.rs` or `Cargo.toml` was changed. `CARGO_TARGET_DIR=target/web-probe` was set on every cargo invocation. Logs: `/tmp/wasm-rpc.log`, `/tmp/wasm-remote.log`, plus keep-going follow-ups `/tmp/wasm-rpc-keepgoing.log` and `/tmp/wasm-remote-keepgoing.log`.

**Toolchain:** rustc 1.97.1 (8bab26f4f 2026-07-14), cargo 1.97.1, target `wasm32-unknown-unknown` already installed. Host: aarch64-apple-darwin.

**Wall time:** the specified commands did not take “ten-plus minutes”. `rpc` failed in ~11s; `remote` in ~26s. Both stopped at a third-party `compile_error!` before type-checking the requested crate. A `--keep-going` follow-up (not in the plan’s command block) was used only to look past that first leaf, which is what §8 Phase 0b asked for: *read past vendor noise to errors in our own files*.

---

## 1. Error totals (specified commands)

Counted as rustc/cargo diagnostics whose line starts with `error[E…]` or `error:`. `cargo:warning=` clang lines are not counted.

| Invocation | `error[E…]` | `error:` | Total | Reached `crates/{rpc,remote}`? |
| --- | ---: | ---: | ---: | --- |
| `cargo check -p rpc --target wasm32-unknown-unknown` | **0** | **2** | **2** | No |
| `cargo check -p remote --target wasm32-unknown-unknown` | **0** | **2** | **2** | No |

### `rpc` — both diagnostics

```
error: the wasm*-unknown-unknown targets are not supported by default, you may need to enable the "js" feature. For more information see: https://docs.rs/getrandom/#webassembly-support
   --> …/getrandom-0.2.16/src/lib.rs:346:9
    |
346 | /         compile_error!("the wasm*-unknown-unknown targets are not supported by \
347 | |                         default, you may need to enable the \"js\" feature. \
…");

error: could not compile `getrandom` (lib) due to 1 previous error
```

### `remote` — both diagnostics

```
error: The target OS is "unknown" or "none", so it's unsupported by the errno crate.
 --> …/errno-0.3.14/src/sys.rs:8:1
  |
8 | compile_error!("The target OS is \"unknown\" or \"none\", so it's unsupported by the errno crate.");

error: could not compile `errno` (lib) due to 1 previous error
```

`remote` got further into the graph than `rpc` (it type-checked several of our leaf crates — `scheduler`, `rope`, `clock`, `text`, … — before `errno` aborted the unit). Neither package’s own sources were type-checked.

---

## 2. Error taxonomy

Two layers, because they answer different questions.

### 2.1 Specified commands (the plan’s deliverable)

Everything is **(a) not-yet-vendored third-party**. **(b) `crates/` is empty.**

| Class | Crate | Code | Count | Site | Root cause |
| --- | --- | --- | ---: | --- | --- |
| (a) | `getrandom` 0.2.16 | `error:` (`compile_error!`) | 1 + 1 wrap | `getrandom-0.2.16/src/lib.rs:346` | wasm32-unknown-unknown needs the `js` feature; default backend refuses the target |
| (a) | `errno` 0.3.14 | `error:` (`compile_error!`) | 1 + 1 wrap | `errno-0.3.14/src/sys.rs:8` | `target_os = "unknown"` has no errno backend |

This is itself a Phase 0b finding: **the probe cannot see `rpc` / `remote` until Phase 2 (or equivalent feature/cfg wiring for `getrandom` / `errno`) exists.** That matches the plan’s “if it turns out it needs the vendored forks, that is a finding for day one.”

### 2.2 `--keep-going` follow-up (reading past the first leaf)

Same target, same `CARGO_TARGET_DIR`, added only `--keep-going`. Still **no** `crates/rpc` and **no** `crates/remote` sources, and **no** `crates/remote/src/claude_sessions.rs`.

**`rpc --keep-going`:** 0 × `error[E…]`, 5 × `error:` (all (a)).

| Class | Crate | Code | Count | Site | Root cause |
| --- | --- | --- | ---: | --- | --- |
| (a) | `getrandom` 0.2.16 | `error:` | 1 + 1 wrap | `lib.rs:346` | same as specified `rpc` |
| (a) | `getrandom` 0.3.4 | `error:` | 1 + 1 wrap | `backends.rs:194` | needs `RUSTFLAGS` `--cfg getrandom_backend="wasm_js"`; the crate’s `wasm_js` Cargo feature alone is not enough |
| (a) | `zstd-sys` 2.0.16+zstd.1.5.7 | `error:` (build script) | 1 | clang `--target=wasm32-unknown-unknown` | host clang: `No available targets are compatible with triple "wasm32-unknown-unknown"` (also tried to assemble `huf_decompress_amd64.S`) |

**`remote --keep-going`:** 30 × `error[E…]` + 11 × `error:` = **41**. Split:

**(a) vendor — 29 × `error[E…]` + 10 × `error:` = 39**

| Crate | Codes | Count | Representative site | Root cause |
| --- | --- | ---: | --- | --- |
| `errno` 0.3.14 | `error:` | 1 + 1 wrap | `sys.rs:8` | same as specified `remote` |
| `polling` 3.11.0 | `error:` | 1 + 1 wrap | `lib.rs:118` | `compile_error!("polling does not support this target OS")` |
| `tree-sitter-json` 0.24.8 | `error:` (build script) | 1 | `src/tree_sitter/parser.h:10` | C build: `'stdlib.h' file not found` |
| `tree-sitter` git `43623ec` | `error:` (build script) | 1 | `lib/src/wasm_store.c:315` | C build for wasm32: implicit `printf` / no libc headers. **Answers plan open question 5: this snapshot does not build for `wasm32-unknown-unknown`.** |
| `zstd-sys` 2.0.16 | `error:` (build script) | 1 | clang wasm32 | same as `rpc --keep-going` |
| `async-tar` git `bd3ad6f` | E0432, E0433, E0277, E0599, E0521 | 21 + 1 wrap | `archive.rs:12` `use async_std::fs` | `async_std::fs` is `cfg(not(target_os = "unknown"))`; wasm32 sets `target_os="unknown"`. Matches §4’s “wrapper yes, `real/` no” |
| `trash` git `41c6c800` | E0433, E0599 | 5 + 1 wrap | `src/lib.rs:67` `platform::PlatformTrashContext` | no wasm `platform` module |
| `wasmtime` 48.0.1 | E0432, E0433, E0599 | 3 + 1 wrap | `runtime/vm.rs:120` `sys::mmap` | `mmap` gated on `has_virtual_memory`; wasm path still references it |

**(b) our `crates/` — 1 × `error[E…]` + 1 × `error:` = 2**

| Crate | Codes | Count | Site | Root cause |
| --- | --- | ---: | --- | --- |
| `paths` | E0432 + wrap | 1 + 1 | `crates/paths/src/paths.rs:8` | re-exports `util::paths::home_dir`, which is `#[cfg(not(target_family = "wasm"))]` — the §6.3 HOME hole, spoken by the compiler |

Full (b) diagnostic:

```
error[E0432]: unresolved import `util::paths::home_dir`
  --> crates/paths/src/paths.rs:8:9
   |
 8 | pub use util::paths::home_dir;
   |         ^^^^^^^^^^^^^^^^^^^^^ no `home_dir` in `paths`
   |
note: found an item that was configured out
  --> crates/util/src/paths.rs:24:8
   |
23 | #[cfg(not(target_family = "wasm"))]
   |          ------------------------ the item is gated here
24 | pub fn home_dir() -> &'static PathBuf {
   |        ^^^^^^^^

error: could not compile `paths` (lib) due to 1 previous error
```

Our crates that **did** type-check for wasm32 under `--keep-going` with no error in the log: `refineable`, `scheduler`, `rope`, `cloud_llm_client`, `clock`, `language_model_core`, `text`, `telemetry_events`, `telemetry`, plus (from the specified `remote` run) `gpui_util`, `collections`, `ztracing`, `zlog`, `util`, `path`, `sum_tree`, `http_client`, `gpui_shared_string`. `gpui` was `Compiling` when `errno`/`polling` failed and never produced a `Checking gpui` line, so it is **not** known to have finished.

---

## 3. (b) per file, and `claude_sessions.rs`

| File | `error[E…]` from this probe | Notes |
| --- | ---: | --- |
| `crates/paths/src/paths.rs` | 1 (E0432) | only `crates/` error the compiler emitted |
| `crates/util/src/paths.rs` | 0 (cited as a note) | `home_dir` configured out; the error is reported at the re-export |
| `crates/rpc/src/proto_client.rs` | **0** | never type-checked |
| `crates/remote/src/claude_sessions.rs` | **0** | never type-checked |
| any other `crates/remote/src/*` | **0** | `remote` itself never reached |

**`claude_sessions.rs` vs the plan’s 113.** The compiler’s number for that file is **0**, not 113. That is not a correction of the 113-site `std::fs` / `fs::` census in §6.2 — this probe never compiled the file, so it cannot confirm or deny that count. The 113 figure remains a source-site count. What Phase 0b can say is: **the expected “large block of compiler errors from `claude_sessions.rs` did not appear**, because `crates/remote` is behind the vendor wall.

---

## 4. Open questions

### 4.1 Does wasm32 report the `proto_client.rs` Instant mix as a type error?

**The specified `cargo check -p rpc` did not.** `proto_client.rs` was never type-checked, so there is **no** `error[E0308]` (or any other diagnostic) pointing at that file. Inventing one from a file rustc did not see would be a fabricated result.

Line numbers have also drifted since the plan was written. The mix is still in the file, but not at `:47`:

| What | Plan | This tree |
| --- | --- | --- |
| `use std::time::{Duration, Instant}` | `:47` | **`:21`** |
| `queued_early_messages_since: Option<Instant>` | (implied) | `:120` |
| `let now = cx.background_executor().now()` | (implied) | `:184` |
| `maybe_queue(..., now: Instant)` | (implied) | `:231` |

`BackgroundExecutor::now` is still `web_time::Instant`: `crates/gpui/src/executor.rs:6` is `use scheduler::Instant`, `:196` returns it, and `crates/scheduler/src/clock.rs:5` is `pub use web_time::Instant`. `scheduler` itself type-checked for wasm32 in the `--keep-going` graph.

Because rustc never compiled the mixing site, a **same-pattern probe** was compiled outside the repo (`/tmp/instant-probe`, `web-time = "1.1.0"`, the version already in this graph). Native `cargo check` **succeeds**. `cargo check --target wasm32-unknown-unknown` **fails** with:

```
error[E0308]: mismatched types
   --> src/main.rs:7:18
    |
  7 |     *slot = Some(now);
    |             ---- ^^^ expected `std::time::Instant`, found `web_time::Instant`
    |
    = note: `web_time::Instant` and `std::time::Instant` have similar names, but are actually distinct types
note: `web_time::Instant` is defined in crate `web_time`
   --> …/web-time-1.1.0/src/time/instant.rs:15:1
    |
 15 | pub struct Instant(Duration);
    | ^^^^^^^^^^^^^^^^^^
note: `std::time::Instant` is defined in crate `std`
   --> …/library/std/src/time.rs:157:1
    |
157 | pub struct Instant(time::Instant);
    | ^^^^^^^^^^^^^^^^^^
```

So: **the type split is real on wasm32, and native hides it.** **This probe did not catch `proto_client.rs` doing it**, because `rpc` never type-checked. §5.5’s “one import line” remains the predicted fix; it is not yet a rustc diagnostic on our tree.

### 4.2 Which `cfg` branch does `process_start_times` select on wasm32?

`crates/remote/src/claude_sessions.rs` defines three mutually exclusive bodies:

- `:205` `#[cfg(target_os = "linux")]` — `/proc/<pid>/stat`
- `:236` `#[cfg(all(unix, not(target_os = "linux")))]` — `ps`
- `:278` `#[cfg(not(unix))]` — `HashMap::default()` (comment: every session reported gone)

`cargo check -p remote` never expanded that file. Proof is from rustc’s own cfg set for this target, plus a `/tmp` probe that used **the same three predicates** and `compile_error!` to name the winner.

`rustc --print cfg --target wasm32-unknown-unknown` includes:

```
target_arch="wasm32"
target_family="wasm"
target_os="unknown"
```

and **does not** include `unix` (`unix_cfg_count=0`). Therefore `target_os = "linux"` is false, `unix` is false, `not(unix)` is true.

Probe (`rustc --target wasm32-unknown-unknown /tmp/process_start_times_cfg.rs`):

```
error: selected: not(unix)
  --> /tmp/process_start_times_cfg.rs:16:5
   |
16 |     compile_error!("selected: not(unix)");
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
```

The linux and unix-not-linux arms did **not** fire. **wasm32-unknown-unknown selects the `not(unix)` empty map.** If this function ever ran in the browser, Claude session liveness would mark every session dead, silently, which is the failure mode §6.2 / open question 2 described.

---

## 5. One-sentence conclusion

§6’s ranking (project_manager low–medium, tmux medium, claude_sessions high) was **not tested**: the compiler never reached those crates, and `claude_sessions.rs` contributed **0** errors rather than a 113-site block; **what this probe shows was underestimated is the Phase 2 dependency wall** (`getrandom`, `errno`, `polling`, `tree-sitter@43623ec`, `zstd-sys`, `async-tar`, `wasmtime`, `trash`) plus the shared `home_dir` cfg hole, all of which sit in front of every §6 estimate.
