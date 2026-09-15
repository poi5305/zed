# Phase 2 — review round 2 (vendored forks)

Adversarial review of the four `web/vendor/` crates and `web/Cargo.toml`, against
`docs/web-zed-plan.md` §4, §4.1, §9, §11 and the author's report `docs/phase2-vendored.md`.

- **Date:** 2026-09-15
- **Branch:** `andy/web-version`
- **Round:** 2 (round 1 was `docs/phase1-review-round1.md`)
- **Scope guard:** no file under `crates/` was touched; root `Cargo.toml` was not edited; root
  `Cargo.lock` is byte-identical to `/tmp/lock-before-review2` (`diff` exit 0, shown below);
  `README.md`'s `> [!IMPORTANT]` header was left in place; no commit, no `git stash/checkout/restore/reset`.
- **Gate:** `web/check-workspace-isolation.sh` — **14 assertions before, 28 after, 0 failures.**
  None of round 1's 14 were modified or removed.

## Headline

**The core §4 mandate was executed correctly.** Every claim in `docs/phase2-vendored.md` about the
*content* of the four forks reproduces exactly:

| Claim | Verdict | Evidence |
| --- | --- | --- |
| ACP `src/` byte-identical to crates.io 2.0.0 across 51 files, only `lib.rs` differs, by 4 `cfg` lines | **true** | `diff -rq` names only `src/lib.rs`; the 50 other files hash to `1e01f690fa5e…`; removing the 4 authorised lines makes `lib.rs` identical too |
| `url_wasm` carries no rustfmt noise | **true** | `diff -rq` against registry `url-2.5.7` names only `src/lib.rs`; `+76/−47`, all of it the wasm branches and the `cfg` wrappers they force |
| `smol_wasm` native path is a re-export, not a hand copy | **true** | the entire non-wasm half of `src/lib.rs` is one line, `pub use smol_real::*;` |
| `wasm_thread_patch` native is `0cf96c77` | **true** | `Cargo.toml` byte-identical; exactly 4 files differ; both `pub use std::thread::…` lines verbatim |
| §4.1's wall: dropping `async-io`/`async-process` removes `errno`/`polling` | **true** | the web wasm32 graph (183 packages) contains none of `async-io`, `async-process`, `polling`, `errno`, `rustix`, `blocking` |

So the answer to this round's headline question — *did a vendored fork silently roll a dependency
backwards?* — is **no, in the crate sources**. The regressions found are one target-cfg that is
wider than §4 authorised, and four integration-level gaps that Phase 2's validation method could
not have seen, because it never compiled anything: `cargo metadata --no-deps` resolves nothing and
builds nothing.

## Step 1 — findings (frozen before any edit)

```json
[
  {"id":"R2-01","severity":"high","file":"web/vendor/agent_client_protocol_patch/Cargo.toml","line":"104-107, 120-123",
   "claim":"The vendored ACP manifest keeps [[test]] jsonrpc_transport_close and [[test]] protocol_v2, whose sources `use agent_client_protocol_test::…`. That dev-dependency is an unpublished path crate stripped by crates.io normalisation (still visible at Cargo.toml.orig:60). The web workspace therefore does not build with --all-targets, and `cargo test` in web/ is impossible.",
   "why_it_matters":"Phase 2 validated only `cargo metadata --no-deps`, which compiles nothing. A workspace that cannot run `cargo check --all-targets` cannot carry a test gate, and Phases 3-6 land on top of it.",
   "how_to_prove":"cd web && CARGO_TARGET_DIR=…/target/web-probe cargo check --workspace --all-targets → E0432 unresolved import `agent_client_protocol_test`"},

  {"id":"R2-02","severity":"high","file":"web/vendor/url_wasm/src/lib.rs","line":"2555, 2612, 2749 (and their not() twins)",
   "claim":"The three branch selectors are the bare predicate `target_arch = \"wasm32\"`. That is true on wasm32-wasip1, wasm32-wasip2 and wasm32-unknown-emscripten as well as wasm32-unknown-unknown. url 2.5.7 already had real from_file_path/from_directory_path/to_file_path implementations on those targets — `target_os = \"wasi\"` and `unix` were already in the outer any(...) — and the fork silently replaces them with the browser stub.",
   "why_it_matters":"This is §11's first-listed high risk exactly: same crate name, same version 2.5.7, same file count, behaviour quietly different on a target §4 never authorised touching.",
   "how_to_prove":"rustc --print cfg --target wasm32-wasip2 emits target_arch=\"wasm32\" AND target_os=\"wasi\"; a compile_error! probe on cfg(target_arch = \"wasm32\") fires for wasm32-wasip2."},

  {"id":"R2-03","severity":"medium","file":"web/vendor/smol_wasm/Cargo.toml","line":"12",
   "claim":"smol_real is pinned by `tag = \"v2.0.2\"`. Git tags are mutable. The root manifest uses `rev` on 36 of its 37 git URLs and `tag` on none.",
   "why_it_matters":"The native half of the wrapper is the thing §4 demanded be real smol. An unpinned tag means real smol can change under us with no file in this repo changing, and §9's lock check only covers the ROOT lock.",
   "how_to_prove":"grep 'tag = ' Cargo.toml (root) → 0 hits; 1 hit in web/vendor/smol_wasm/Cargo.toml. The resolved commit a1e642196803fc5dff56f1e04e867bfc024966bd is recorded nowhere in the tree."},

  {"id":"R2-04","severity":"medium","file":"web/Cargo.lock","line":"n/a (absent)",
   "claim":"No web/Cargo.lock is committed. §11's own mitigation for 'Two Cargo.lock files drift' is 'Both committed'; it does not exist. A fresh resolve already picks agent-client-protocol-derive 2.1.0 where the root lock pins 2.0.0 — a different proc-macro expanding the byte-identical vendored 2.0.0 source.",
   "why_it_matters":"The entire value of vendoring a byte-identical 2.0.0 is that it is the crate the desktop builds. A floating derive macro removes that guarantee silently.",
   "how_to_prove":"ls web/Cargo.lock → absent; cargo metadata in web/ resolves agent-client-protocol-derive 2.1.0 against root Cargo.lock's 2.0.0."},

  {"id":"R2-05","severity":"low","file":"web/Cargo.toml","line":"43",
   "claim":"[patch.crates-io] wasm_thread is inert. Only [patch.\"https://github.com/zed-industries/wasm_thread\"] patches anything, because crates/gpui_web:37 and crates/scheduler:41 declare wasm_thread from the git URL.",
   "why_it_matters":"It reads as the load-bearing entry. Deleting the git-URL table later leaves a patch that looks complete while wasm_thread silently reverts to unpatched 0cf96c77 — which still carries #![feature(stdarch_wasm_atomic_wait)] and would need nightly.",
   "how_to_prove":"Probe workspace: crates-io patch only → resolves to the git source plus 'patch was not used'; git-URL patch only → resolves to the vendored path."},

  {"id":"R2-06","severity":"low","file":"web/vendor/wasm_thread_patch/src/wasm32/js/web_worker_module.js","line":"4",
   "claim":"prepareSqlRpcBridge() is guarded by `self.name !== \"sqlez-worker\"`, but Builder composes the worker name as \"{prefix}:{name}\" whenever a prefix is set (src/wasm32/mod.rs:284-293). Any caller that uses Builder::prefix makes the bridge silently never start.",
   "why_it_matters":"Silent no-op with no error anywhere; sqlez would fall back with no diagnostic.",
   "how_to_prove":"Read src/wasm32/mod.rs:283-296 — options.set_name(&format!(\"{}:{}\", prefix, name))."},

  {"id":"R2-07","severity":"medium","file":"web/vendor/url_wasm/src/lib.rs","line":"2752-2755",
   "claim":"The wasm to_file_path returns Ok(PathBuf::from(self.path())) for ANY scheme and without percent-decoding. Url::from_file_path(\"/a b\")?.to_file_path() yields \"/a%20b\"; Url::parse(\"https://example.com/x\")?.to_file_path() yields Ok(\"/x\") instead of Err(()).",
   "why_it_matters":"Every caller treats to_file_path() == Ok as 'this is a local file'. Round-trip loss plus scheme confusion.",
   "how_to_prove":"Requires executing wasm32-unknown-unknown code; no runner exists in this tree."}
]
```

No `discoveredWhileFixing` entries. Three harness bugs were found and fixed while writing the
assertions themselves (BSD `diff`'s `Files A and B differ` suffix; `0cf96c77`'s `lib.rs` has **two**
`pub use std::thread::…` lines, not one; the first cfg evaluator substituted bare identifiers inside
the string literals its own key/value substitution had produced). Those are defects in this round's
test code, not in the reviewed code, and are not counted as findings.

## Step 2 — RED

`web/check-workspace-isolation.sh` grew 14 new assertions (V1-V11; V5, V9 and V11 each assert twice).
Nothing existing was changed. On the tree as the author left it:

```
ok   V1 the four vendored packages keep their base name and version
ok   V2 agent_client_protocol_patch/src is byte-identical to crates.io 2.0.0 apart from 4 cfg lines in lib.rs
ok   V3 url_wasm differs from crates.io url 2.5.7 in src/lib.rs only (no rustfmt noise)
FAIL V4 url_wasm's wasm branch selectors fire on wasm32-unknown-unknown only
       expected: wasm32-unknown-unknown only
       actual:   [target_arch = "wasm32"] is TRUE on wasm32-unknown-emscripten; [target_arch = "wasm32"] is TRUE on wasm32-wasip1; [target_arch = "wasm32"] is TRUE on wasm32-wasip2
ok   V5 wasm_thread_patch differs from git 0cf96c77 in the four §4 files only
ok   V5 wasm_thread_patch keeps 0cf96c77's native 'pub use std::thread::…' verbatim
ok   V6 smol_wasm's native half is a re-export of real smol, not a hand copy
FAIL V7 every git dependency inside web/vendor is pinned by rev, not a mutable tag/branch
       expected: none
       actual:   smol_wasm:smol_real(tag=v2.0.2)
FAIL V8 each vendored package is patched on the source kind the root workspace resolves it from, with no inert twin
       expected: none
       actual:   wasm_thread: root source is git, so its [patch.crates-io] entry is inert
FAIL V9 web/Cargo.lock exists, so the web graph is pinned at all
       expected: /Users/andy/go/src/github.com/poi5305/zed/web/Cargo.lock
       actual:   absent — the web workspace re-resolves from scratch on every build
FAIL V9 the vendored agent-client-protocol is built by the companion crates the desktop builds it with
       expected: none
       actual:   agent-client-protocol-derive: root=2.0.0 web=2.1.0
ok   V10 the web wasm32 graph is free of async-io/async-process/polling/errno/rustix (§4.1's wall)
FAIL V11 cd web && cargo check --workspace --all-targets succeeds
       expected: exit 0
       actual:   exit 101
       error[E0432]: unresolved import `agent_client_protocol_test` error: could not compile `agent-client-protocol` (test "jsonrpc_transport_close") due to 1 previous error

28 checks, 5 failures
```

(The first RED run showed 9 failures; four of those were the harness bugs above. The 5 above are the
real ones, and each names the actual value against the expected one.)

### The six assertions that were green from the start are not vacuous

Proven by break → red → restore, all six broken in one pass and restored from a snapshot afterwards:

| Assertion | Deliberate breakage | Observed |
| --- | --- | --- |
| V1 | `smol_wasm` version `2.0.2` → `2.0.3` | FAIL |
| V2 | one byte appended to `agent_client_protocol_patch/src/session.rs` | FAIL, `hash=c4517029c45b…` vs `hash=1e01f690fa5e…` |
| V3 | one line appended to `url_wasm/README.md` | FAIL, `actual: README.md,src/lib.rs` |
| V5 | one line appended to `wasm_thread_patch/src/wasm32/utils.rs` | FAIL, fifth file `src/wasm32/utils.rs` listed |
| V6 | a second native item added beside the re-export | FAIL, `native_code=['pub use smol_real::*;', 'pub fn spawn_hand_copy() {}']` |
| V10 | `async-io = "2"` added to `smol_wasm`'s wasm dependency table | FAIL, `actual: async-io,errno,polling,rustix` — §4.1's edge, reproduced |

V4, V7, V8 and V9 stayed green under those mutations, so the six are independent.

## Step 3 — fixes, and the reconciliation

| Finding | Disposition | Change |
| --- | --- | --- |
| R2-01 | **fixed** | `web/vendor/agent_client_protocol_patch/Cargo.toml`: dropped the `jsonrpc_transport_close` and `protocol_v2` `[[test]]` stanzas, with a comment naming `Cargo.toml.orig:60` as the reason. The 14 other test targets and both examples still build. |
| R2-02 | **fixed** | `web/vendor/url_wasm/src/lib.rs`: six selectors narrowed from `target_arch = "wasm32"` to `all(target_arch = "wasm32", target_os = "unknown")`. The outer `any(...)` entries are left as the author wrote them — on wasi and emscripten they were already redundant, so they change nothing there and are needed for the methods to exist at all on `wasm32-unknown-unknown`. |
| R2-03 | **fixed** | `web/vendor/smol_wasm/Cargo.toml`: `tag = "v2.0.2"` → `rev = "a1e642196803fc5dff56f1e04e867bfc024966bd"` (the peeled commit the tag resolved to), matching the root manifest's convention. |
| R2-04 | **fixed** | `web/vendor/agent_client_protocol_patch/Cargo.toml`: `agent-client-protocol-derive` pinned `=2.0.0`, as upstream already pins `-schema` at `=1.5.0`. `web/Cargo.lock` regenerated and now present. |
| R2-05 | **fixed** | `web/Cargo.toml`: the inert `[patch.crates-io] wasm_thread` entry removed; a comment records the measurement and points at the git-URL table that does the work. |
| R2-06 | **wontfix** | No caller exists until Phase 4 — `wasm_thread::Builder` is not used anywhere in `web/` yet. Recorded here as a Phase 4 contract: **the sqlez worker must be built with `Builder::name("sqlez-worker")` and no `prefix`, or `prepareSqlRpcBridge()` never runs and says nothing.** |
| R2-07 | **wontfix** | Cannot be turned red. The code only compiles for `wasm32-unknown-unknown`, and this tree has no way to execute it — no `wasm-bindgen-test` runner, no browser harness. Writing a fix without a failing test would violate this loop's own rule. **Phase 5 ("first light") must add the wasm test harness and then assert: `Url::from_file_path("/a b")?.to_file_path() == "/a b"` and `Url::parse("https://example.com/x")?.to_file_path().is_err()`.** Both fail today. |

**Reconciliation:** 7 findings = 5 fixed + 2 `wontfix`. 5 RED assertions ↔ the 5 fixed findings, one
each (R2-01→V11, R2-02→V4, R2-03→V7, R2-04→V9, R2-05→V8). Every edit outside `docs/` maps to a row
above; the only other changed file is `web/check-workspace-isolation.sh` itself, which is this round's
test artefact.

### What each new boundary could wrongly kill, and why it does not

- **R2-02's narrowed cfg** could in principle remove the wasm stub from a target that needs it. It
  does not: `cargo check --workspace --target wasm32-unknown-unknown` still succeeds (the stub is
  still selected there), and `cargo check -p url --target wasm32-wasip2` now succeeds *taking the
  real implementation* — which is the behaviour url 2.5.7 shipped.
- **R2-01's removed test targets** could drop coverage. They compiled on no target at all before
  this change, so nothing is lost; the other 14 ACP test targets and 2 examples now build.
- **R2-04's `=2.0.0`** could make the graph unresolvable if something in the web workspace wanted
  `^2.1`. Nothing does: the re-resolve succeeds and reports no conflict.
- **R2-05's removal** would matter if some crate took `wasm_thread` from crates.io. None does, and
  V8 re-derives that from the root `Cargo.lock` on every run rather than trusting today's answer.

## Step 3 — GREEN

```
28 checks, 0 failures
```

All 14 round-1 assertions still pass, unmodified.

## §9 evidence

```
diff /tmp/lock-before-review2 /Users/andy/go/src/github.com/poi5305/zed/Cargo.lock
diff_exit:0
```

Empty, before and after the fixes. No file under `crates/` was modified; `git diff --stat` shows only
the pre-existing `Cargo.toml` (`exclude = ["web"]`, Phase 1) and `docs/web-zed-plan.md` changes, neither
of them this round's. Every cargo invocation that can write artefacts used
`CARGO_TARGET_DIR=…/target/web-probe`. No `./script/clippy`, no `--release`, no `--all-features`.

## Two things the next phase inherits

1. **`web/Cargo.lock` now exists and must be committed.** It is 223 packages, none of which the
   desktop graph sees. It also makes the `agent-client-protocol-derive` pin durable.
2. **Three `[patch.crates-io]` entries in `web/Cargo.toml` are currently unused** — `notify`,
   `notify-types`, `tree-sitter-language` — and cargo says so on every resolve. That is correct for
   now (nothing in the web workspace reaches them until Phase 3 adds `gpui`/`rpc`/`project`), but the
   warnings will mask a *real* unused-patch warning if one appears. Worth a Phase 3 assertion that the
   set of unused patches is exactly the set still waiting for their consumer.

## Suggested .rules additions

None. Nothing here generalises beyond this fork's `web/` workspace.
