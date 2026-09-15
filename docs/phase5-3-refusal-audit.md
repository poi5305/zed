# Phase 5.3 — Post-Merge Refusal Audit Report

- **Date:** 2026-09-15
- **Branch:** `andy/web-version`
- **Script:** [`web/check-refusals.sh`](file:///Users/andy/go/src/github.com/poi5305/zed/web/check-refusals.sh)

---

## 1. Background and Rationale

In `docs/web-zed-plan.md` §5.3, four changes from `zedweb/zed-web` are classified as strictly refused:
1. `crates/zed/RELEASE_CHANNEL`: `dev` → `stable`
2. `crates/terminal/src/terminal.rs`: Shift+Click selection extension deleted
3. `crates/recent_projects/src/recent_projects.rs`: `PathPromptOptions.files: true` → `false`
4. `crates/remote_server/src/server.rs`: log flush `send_blocking` → `try_send`

### Why an automated post-merge audit is necessary

As measured in Phase 0 (`docs/phase0-rebase-cost.md`), **three of the four changes do not conflict** during git merge and land silently into the merged worktree. The fourth (`recent_projects.rs`) conflicts on an unrelated dev-container hunk, but the `files: false` mutation auto-merges cleanly at merged line 2164.

Because git merge does not stop on these modifications, a refusal list cannot function as a pre-merge or merge-time filter. It must be implemented as a mechanical **post-merge audit** running against our tree, executable after every rebase or upstream sync.

---

## 2. Script Implementation Details

The audit script [`web/check-refusals.sh`](file:///Users/andy/go/src/github.com/poi5305/zed/web/check-refusals.sh) is modeled directly on [`web/check-workspace-isolation.sh`](file:///Users/andy/go/src/github.com/poi5305/zed/web/check-workspace-isolation.sh):
- **Root-relative execution:** Resolves `repo_dir` dynamically via `$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)` so it can be run from any working directory.
- **Output convention:** Uses `ok <label>` and `FAIL <label>` with indented `expected:` and `actual:` diagnostics.
- **No `timeout` command:** Relies exclusively on portable POSIX shell and Python 3 standard library (`re`, `sys`, `os`), adhering to the machine environment.
- **Exit code contract:** Returns exit status 0 if and only if all checks pass (`failures == 0`); returns non-zero if any assertion fails.

---

## 3. Assertion Design and Anti-Drift Analysis

Each of the four assertions is bound to syntactic and behavioral properties rather than fragile line numbers.

### Assertion 1: `crates/zed/RELEASE_CHANNEL`
- **Label:** `§5.3.1 RELEASE_CHANNEL is dev`
- **Binding:** Reads the trimmed content of [`crates/zed/RELEASE_CHANNEL`](file:///Users/andy/go/src/github.com/poi5305/zed/crates/zed/RELEASE_CHANNEL).
- **Expected:** `dev`
- **Why it will not falsely pass or fail:** The file consists only of the channel name. Reading with whitespace stripping handles both `dev\n` and raw `dev`, while immediately catching `stable` (or any other channel override).

### Assertion 2: `crates/terminal/src/terminal.rs` (Shift+Click Selection Extension)
- **Label:** `§5.3.2 terminal Shift+Click selection extension exists`
- **Code Location:** `Terminal::mouse_down` handling left clicks.
- **Semantic Context:** Upstream commits `#25143` and `#60880` established that when `selection_type == Some(SelectionType::Simple) && e.modifiers.shift`, the terminal must test whether an existing selection exists (`self.last_content.selection.is_some()`):
  - If true: extend via `self.events.push_back(InternalEvent::UpdateSelection(position))`
  - If false: drop an anchor via `self.events.push_back(InternalEvent::SetSelection(Some(Selection::new(SelectionType::Simple, point, side))))`
  `zed-web` removed this conditional branching entirely, replacing it with an unconditional `UpdateSelection` push.
- **Binding:** Python parser locates the pattern `if\s+selection_type\s*==\s*Some\(SelectionType::Simple\)\s*&&\s*e\.modifiers\.shift`, extracts the balanced-brace block `{ ... }`, and inspects the behavioral constructs within that exact block:
  - Checks for `self.last_content.selection.is_some()`
  - Checks for `InternalEvent::UpdateSelection`
  - Checks for `InternalEvent::SetSelection`
- **Why it will not falsely pass or fail:**
  - **No line numbers:** Does not care if lines above or below drift during upstream rebases.
  - **Scoped to handler:** Uses balanced brace matching to inspect only the Shift+Click branch.
  - **Behavior-bound:** If `zed-web`'s 4-line deletion lands, `last_content.selection.is_some()` is absent from the handler, causing the assertion to report `deleted (unconditional UpdateSelection without selection.is_some check)` and fail.

### Assertion 3: `crates/recent_projects/src/recent_projects.rs` (`open_local_project`)
- **Label:** `§5.3.3 recent_projects open_local_project PathPromptOptions.files is true`
- **Semantic Context:** In `fn open_local_project`, `PathPromptOptions` must have `files: true` so the desktop project picker allows picking a single file. `zed-web` changed this to `files: false`.
- **Distinction Requirement:** The same file contains another `PathPromptOptions` for `OpenWslFolder` (`with_active_or_new_workspace`) at line 298, which `zed-web` did *not* change (`files: true`). The assertion must distinguish these two sites.
- **Binding:** Locates `fn open_local_project(`, extracts the function body using balanced brace parsing, and only within that function searches for `PathPromptOptions { ... }` and parses `files: (true|false)`.
- **Why it will not falsely pass or fail:**
  - The WSL site is outside `fn open_local_project` and is completely ignored. Even if the WSL prompt is edited or mutated, Assertion 3 remains strictly bound to `open_local_project`.
  - If `open_local_project` is mutated to `files: false`, it fails immediately.
  - If the function or struct is renamed or missing, it emits `<fn open_local_project not found>` and fails.

### Assertion 4: `crates/remote_server/src/server.rs` (`MultiWrite::flush`)
- **Label:** `§5.3.4 remote_server MultiWrite::flush uses send_blocking`
- **Semantic Context:** In `init_logging_server`, `MultiWrite::flush` flushes buffered log records into `self.channel` (`async_channel::Sender<Vec<u8>>`). Desktop requires `send_blocking` to avoid losing log records on backpressure. `zed-web` changed this to `try_send`, making log emission lossy.
- **Binding:** Locates `impl Write for MultiWrite`, extracts its body with balanced braces, locates `fn flush(`, extracts the flush body with balanced braces, and extracts the channel method invocation `self.channel.<method>(...)`.
- **Why it will not falsely pass or fail:**
  - Scoped to `MultiWrite::flush` specifically, independent of line numbers (~255) and unrelated to other channel usages in `server.rs`.
  - Distinguishes `send_blocking` from `try_send`.

---

## 4. Verification and Real Terminal Output

### 4.1 Baseline: All Four Pass on Current Tree

Executing `./web/check-refusals.sh` on our current tree (`andy/web-version`):

```console
$ ./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
Exit status: 0
```

---

### 4.2 Round 1: Breaking Item 1 (`RELEASE_CHANNEL` → `stable`)

Temporarily mutated `crates/zed/RELEASE_CHANNEL` to `stable`:

```console
$ ./web/check-refusals.sh
FAIL §5.3.1 RELEASE_CHANNEL is dev
       expected: dev
       actual:   stable
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 1 failures
Exit status: 1
```

*Result:* Exactly check 1 failed with expected vs actual values; checks 2, 3, and 4 remained green. Reverted and confirmed clean.

---

### 4.3 Round 2: Breaking Item 2 (`terminal.rs` → zed-web unconditional `UpdateSelection`)

Temporarily replaced lines 2692–2706 of `crates/terminal/src/terminal.rs` with `zed-web`'s version:
```rust
                    if selection_type == Some(SelectionType::Simple) && e.modifiers.shift {
                        self.events
                            .push_back(InternalEvent::UpdateSelection(position));
                        return;
                    }
```

Running `./web/check-refusals.sh`:

```console
$ ./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
FAIL §5.3.2 terminal Shift+Click selection extension exists
       expected: present (last_content.selection.is_some ? UpdateSelection : SetSelection)
       actual:   deleted (unconditional UpdateSelection without selection.is_some check)
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 1 failures
Exit status: 1
```

*Result:* Exactly check 2 failed, clearly identifying the deleted conditional selection branch. Checks 1, 3, and 4 remained green. Reverted and confirmed clean.

---

### 4.4 Round 3: Breaking Item 3 (`recent_projects.rs` → `files: false`)

#### Part A: Mutating `open_local_project` to `files: false`

Temporarily changed `open_local_project`'s `PathPromptOptions.files` to `false`:

```console
$ ./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
FAIL §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
       expected: true
       actual:   false
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 1 failures
Exit status: 1
```

*Result:* Exactly check 3 failed; checks 1, 2, and 4 remained green. Reverted and confirmed clean.

#### Part B: Counter-test on WSL Prompt (`OpenWslFolder`)

Temporarily changed the WSL `PathPromptOptions.files` (line 298) to `false`:

```console
$ ./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
Exit status: 0
```

*Result:* Check 3 remained green, proving that the assertion correctly distinguishes `open_local_project` from the WSL site. Reverted and confirmed clean.

---

### 4.5 Round 4: Breaking Item 4 (`remote_server/src/server.rs` → `try_send`)

Temporarily changed `send_blocking` to `try_send` in `MultiWrite::flush`:

```console
$ ./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
FAIL §5.3.4 remote_server MultiWrite::flush uses send_blocking
       expected: send_blocking
       actual:   try_send

4 checks, 1 failures
Exit status: 1
```

*Result:* Exactly check 4 failed with expected vs actual values; checks 1, 2, and 3 remained green. Reverted and confirmed clean.

---

### 4.6 Final Clean Verification and Tree State

Running `./web/check-refusals.sh` after all reversions:

```console
$ ./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
Exit status: 0
```

Verifying that no changes were left in `crates/`:

```console
$ git diff crates/
(empty)

$ git status --short crates/
(empty)
```

All four refusal invariants are verified, robust against code drift, and permanently safeguarded by `web/check-refusals.sh`.
