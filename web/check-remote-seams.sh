#!/usr/bin/env bash
# Verifies the remote SSH seams between the web workspace, zed-web-server, and the remote client.
#
# WHY THIS EXISTS: components across crates and scripts meet only as string names,
# path literals, and cfg gates. cargo check accepts both sides after one side is
# commented out, cfg'd out, or moved to a different function.

set -euo pipefail

# Anchor a relative override at this script's repository. Resolving it from the
# caller's cwd would let the same command check a different tree after cd.
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
default_root="$(cd "${script_dir}/.." && pwd)"
if [[ -n "${ZED_REMOTE_SEAMS_ROOT:-}" ]]; then
    case "${ZED_REMOTE_SEAMS_ROOT}" in
        /*) repo_dir="${ZED_REMOTE_SEAMS_ROOT}" ;;
        *) repo_dir="${default_root}/${ZED_REMOTE_SEAMS_ROOT}" ;;
    esac
else
    repo_dir="${default_root}"
fi

if ! command -v python3 >/dev/null 2>&1; then
    printf 'FAIL remote seam check needs python3\n' >&2
    exit 1
fi

python3 - "${repo_dir}" << 'PY'
import os
import re
import sys

sys.stdout.reconfigure(line_buffering=True)
sys.stderr.reconfigure(line_buffering=True)

root = sys.argv[1]
failures = 0

TARGETS = {
    "wasm": {"family": "wasm", "os": "unknown", "unix": False, "windows": False},
    "linux": {"family": "unix", "os": "linux", "unix": True, "windows": False},
    "macos": {"family": "unix", "os": "macos", "unix": True, "windows": False},
    "windows": {"family": "windows", "os": "windows", "unix": False, "windows": True},
}


def fail(message):
    global failures
    failures += 1
    print(message, file=sys.stderr)


def read_text(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read()
    except OSError as error:
        fail(f"FAIL cannot read {path}: {error}")
        return None


def mask_rust(src):
    """Code characters kept; comments and string/char literals blanked.

    Indices match src. Braces inside literals must not affect matching, and a
    // or #[cfg] that exists only in a comment must not count as a seam.
    """
    n = len(src)
    out = list(src)
    i = 0

    def blank(start, end):
        for index in range(start, end):
            if out[index] != "\n":
                out[index] = " "

    def raw_end(start):
        j = start
        if j < n and src[j] in "bc":
            j += 1
        if j >= n or src[j] != "r":
            return None
        j += 1
        hashes = 0
        while j < n and src[j] == "#":
            hashes += 1
            j += 1
        if j >= n or src[j] != '"':
            return None
        j += 1
        terminator = '"' + ("#" * hashes)
        found = src.find(terminator, j)
        if found < 0:
            return n
        return found + len(terminator)

    def string_end(quote):
        j = quote + 1
        while j < n:
            if src[j] == "\\":
                j += 2
                continue
            if src[j] == '"':
                return j + 1
            j += 1
        return n

    def char_end(start):
        if src[start] != "'":
            return None
        j = start + 1
        if j >= n or src[j] == "\n":
            return None
        if src[j] == "\\":
            j += 1
            if j >= n:
                return None
            if src[j] == "u" and j + 1 < n and src[j + 1] == "{":
                j += 2
                while j < n and src[j] != "}":
                    j += 1
                j += 1
            elif src[j] == "x":
                j += 1
                while j < n and j < start + 6 and src[j] in "0123456789abcdefABCDEF":
                    j += 1
            else:
                j += 1
            if j < n and src[j] == "'":
                return j + 1
            return None
        if j + 1 < n and src[j + 1] == "'":
            return j + 2
        return None

    while i < n:
        if src.startswith("//", i):
            end = src.find("\n", i)
            blank(i, n if end < 0 else end)
            i = n if end < 0 else end
            continue
        if src.startswith("/*", i):
            end = src.find("*/", i + 2)
            end = n if end < 0 else end + 2
            blank(i, end)
            i = end
            continue
        raw = raw_end(i)
        if raw is not None:
            blank(i, raw)
            i = raw
            continue
        if src[i] in "bc" and i + 1 < n and src[i + 1] == '"':
            end = string_end(i + 1)
            blank(i, end)
            i = end
            continue
        if src[i] == '"':
            end = string_end(i)
            blank(i, end)
            i = end
            continue
        char = char_end(i)
        if char is not None:
            blank(i, char)
            i = char
            continue
        i += 1
    return "".join(out)


def match_pair(masked, open_index, open_char, close_char):
    depth = 0
    for index in range(open_index, len(masked)):
        char = masked[index]
        if char == open_char:
            depth += 1
        elif char == close_char:
            depth -= 1
            if depth == 0:
                return index
    return None


def function_body(masked, after_name):
    paren = 0
    index = after_name
    while index < len(masked):
        char = masked[index]
        if char == "(":
            paren += 1
        elif char == ")":
            paren -= 1
        elif char == "{" and paren == 0:
            end = match_pair(masked, index, "{", "}")
            if end is None:
                return None, None
            return index, end
        index += 1
    return None, None


def strip_angles(text):
    out = []
    depth = 0
    for char in text:
        if char == "<":
            depth += 1
        elif char == ">":
            depth = max(0, depth - 1)
        elif depth == 0:
            out.append(char)
    return "".join(out)


def impl_type_is(header, type_name):
    header = re.sub(r"\s+", " ", strip_angles(header)).strip()
    match = re.search(r"\bfor\s+([A-Za-z0-9_]+)\s*$", header)
    if match:
        return match.group(1) == type_name
    match = re.search(r"\bimpl\s+([A-Za-z0-9_]+)\s*$", header)
    return bool(match and match.group(1) == type_name)


def impl_bodies(masked, type_name):
    bodies = []
    for match in re.finditer(r"\bimpl\b", masked):
        index = match.end()
        paren = 0
        bracket = 0
        while index < len(masked):
            char = masked[index]
            if char == "(":
                paren += 1
            elif char == ")":
                paren -= 1
            elif char == "[":
                bracket += 1
            elif char == "]":
                bracket -= 1
            elif char == "{" and paren == 0 and bracket == 0:
                end = match_pair(masked, index, "{", "}")
                if end is not None and impl_type_is(masked[match.start() : index], type_name):
                    bodies.append((index, end))
                break
            elif char == ";" and paren == 0 and bracket == 0:
                break
            index += 1
    return bodies


def functions_named(masked, name, within):
    start, end = within
    found = []
    region = masked[start : end + 1]
    for match in re.finditer(r"\bfn\s+" + re.escape(name) + r"\b", region):
        absolute = start + match.end()
        body_start, body_end = function_body(masked, absolute)
        found.append((start + match.start(), body_start, body_end))
    return found


def skip_space(masked, index):
    while index < len(masked) and masked[index] in " \t\n\r":
        index += 1
    return index


def item_end_from(masked, index):
    paren = 0
    bracket = 0
    while index < len(masked):
        char = masked[index]
        if char == "(":
            paren += 1
        elif char == ")":
            paren = max(0, paren - 1)
        elif char == "[":
            bracket += 1
        elif char == "]":
            bracket = max(0, bracket - 1)
        elif char == "{" and paren == 0 and bracket == 0:
            end = match_pair(masked, index, "{", "}")
            return len(masked) if end is None else end + 1
        elif char == ";" and paren == 0 and bracket == 0:
            return index + 1
        index += 1
    return len(masked)


def extract_cfg(text):
    match = re.fullmatch(r"\[\s*cfg\s*\((.*)\)\s*\]", text, re.DOTALL)
    if not match:
        return None
    return match.group(1).strip()


def cfg_spans(src, masked):
    spans = []
    n = len(masked)
    index = 0
    while index < n:
        if masked[index] == "#" and index + 1 < n and masked[index + 1] == "[":
            exprs = []
            while (
                index < n
                and masked[index] == "#"
                and index + 1 < n
                and masked[index + 1] == "["
            ):
                if index + 2 < n and masked[index + 2] == "!":
                    close = match_pair(masked, index + 2, "[", "]")
                    index = n if close is None else close + 1
                    index = skip_space(masked, index)
                    continue
                close = match_pair(masked, index + 1, "[", "]")
                if close is None:
                    index += 1
                    break
                expr = extract_cfg(src[index + 1 : close + 1])
                if expr is not None:
                    exprs.append(expr)
                index = skip_space(masked, close + 1)
            end = item_end_from(masked, index)
            for expr in exprs:
                spans.append((index, end, expr))
            continue
        index += 1
    return spans


class CfgParser:
    def __init__(self, text):
        self.text = text
        self.n = len(text)
        self.i = 0

    def skip(self):
        while self.i < self.n and self.text[self.i].isspace():
            self.i += 1

    def peek(self):
        self.skip()
        if self.i >= self.n:
            return ""
        return self.text[self.i]

    def starts(self, word):
        self.skip()
        if not self.text.startswith(word, self.i):
            return False
        after = self.i + len(word)
        if after < self.n and (self.text[after].isalnum() or self.text[after] == "_"):
            return False
        return True

    def eat(self, word):
        if not self.starts(word):
            raise ValueError(word)
        self.i += len(word)

    def parse_ident(self):
        self.skip()
        start = self.i
        while self.i < self.n and (self.text[self.i].isalnum() or self.text[self.i] == "_"):
            self.i += 1
        if start == self.i:
            raise ValueError("ident")
        return self.text[start : self.i]

    def parse_string(self):
        self.skip()
        if self.peek() != '"':
            raise ValueError("string")
        self.i += 1
        chars = []
        while self.i < self.n:
            char = self.text[self.i]
            if char == "\\":
                self.i += 2
                continue
            if char == '"':
                self.i += 1
                return "".join(chars)
            chars.append(char)
            self.i += 1
        raise ValueError("string end")

    def parse_args(self):
        self.skip()
        if self.peek() != "(":
            raise ValueError("args")
        self.i += 1
        args = []
        self.skip()
        if self.peek() == ")":
            self.i += 1
            return args
        while True:
            args.append(self.parse())
            self.skip()
            if self.peek() == ",":
                self.i += 1
                continue
            if self.peek() == ")":
                self.i += 1
                return args
            raise ValueError("arg sep")

    def parse(self):
        if self.starts("any"):
            self.eat("any")
            return ("any", self.parse_args())
        if self.starts("all"):
            self.eat("all")
            return ("all", self.parse_args())
        if self.starts("not"):
            self.eat("not")
            self.skip()
            if self.peek() != "(":
                raise ValueError("not")
            self.i += 1
            inner = self.parse()
            self.skip()
            if self.peek() != ")":
                raise ValueError("not close")
            self.i += 1
            return ("not", inner)
        ident = self.parse_ident()
        self.skip()
        if self.peek() == "=":
            self.i += 1
            return ("eq", ident, self.parse_string())
        return ("ident", ident)


def eval_node(node, target):
    info = TARGETS[target]
    kind = node[0]
    if kind == "any":
        # Empty any() is never compiled. That is the #[cfg(any())] gate.
        if not node[1]:
            return False
        values = [eval_node(item, target) for item in node[1]]
        if any(value is True for value in values):
            return True
        if all(value is False for value in values):
            return False
        return None
    if kind == "all":
        if not node[1]:
            return True
        values = [eval_node(item, target) for item in node[1]]
        if any(value is False for value in values):
            return False
        if all(value is True for value in values):
            return True
        return None
    if kind == "not":
        value = eval_node(node[1], target)
        if value is True:
            return False
        if value is False:
            return True
        return None
    if kind == "eq":
        key, value = node[1], node[2]
        if key == "target_family":
            return info["family"] == value
        if key == "target_os":
            return info["os"] == value
        return None
    if kind == "ident":
        if node[1] == "unix":
            return info["unix"]
        if node[1] == "windows":
            return info["windows"]
        return None
    return None


def eval_expr(expr, target):
    parser = CfgParser(expr)
    try:
        node = parser.parse()
        parser.skip()
        if parser.i != parser.n:
            return None
    except ValueError:
        return None
    return eval_node(node, target)


def covered(spans, pos, target):
    result = True
    for start, end, expr in spans:
        if start <= pos < end:
            value = eval_expr(expr, target)
            if value is False:
                return False
            if value is None:
                result = None
    return result


def live_call(masked, body_start, body_end, needle, predicate):
    if body_start is None or body_end is None:
        return False
    body = masked[body_start : body_end + 1]
    for match in re.finditer(re.escape(needle), body):
        if predicate(body_start + match.start()):
            return True
    return False


def require_file(path, label):
    if os.path.isfile(path):
        return True
    fail(f"FAIL {label}: {path} does not exist")
    return False


def shell_assignments(src):
    """Last top-level remote_dir and manifest.

    An assignment inside a function is not the path the script writes on a
    normal run, so brace-group depth must be zero. ${...} is not a group.
    """
    remote = None
    manifest = None
    n = len(src)
    index = 0
    lex = ["code"]
    struct = []

    def group_depth():
        return sum(1 for kind in struct if kind == "group")

    def paren_depth():
        return sum(1 for kind in struct if kind == "paren")

    while index < n:
        mode = lex[-1]
        char = src[index]
        if mode == "sq":
            if char == "'":
                lex.pop()
            index += 1
            continue
        if mode == "dq":
            if char == "\\" and index + 1 < n:
                index += 2
                continue
            if char == '"':
                lex.pop()
                index += 1
                continue
            if char == "$" and index + 1 < n and src[index + 1] == "(":
                lex.append("code")
                struct.append("paren")
                index += 2
                continue
            if char == "$" and index + 1 < n and src[index + 1] == "{":
                struct.append("param")
                index += 2
                continue
            if char == "}" and struct and struct[-1] == "param":
                struct.pop()
                index += 1
                continue
            index += 1
            continue
        if char == "\\" and index + 1 < n and src[index + 1] == "\n":
            index += 2
            continue
        if char == "'":
            lex.append("sq")
            index += 1
            continue
        if char == '"':
            lex.append("dq")
            index += 1
            continue
        if char == "#":
            while index < n and src[index] != "\n":
                index += 1
            continue
        if char == "$" and index + 1 < n and src[index + 1] == "(":
            struct.append("paren")
            index += 2
            continue
        if char == "$" and index + 1 < n and src[index + 1] == "{":
            struct.append("param")
            index += 2
            continue
        if char == "(":
            struct.append("paren")
            index += 1
            continue
        if char == ")" and struct and struct[-1] == "paren":
            struct.pop()
            if len(lex) > 1 and lex[-1] == "code":
                lex.pop()
            index += 1
            continue
        if char == "{" and not (struct and struct[-1] == "param"):
            struct.append("group")
            index += 1
            continue
        if char == "}":
            if struct and struct[-1] in ("group", "param"):
                struct.pop()
            index += 1
            continue
        if (
            group_depth() == 0
            and paren_depth() == 0
            and (char.isalpha() or char == "_")
            and (index == 0 or not (src[index - 1].isalnum() or src[index - 1] == "_"))
        ):
            end = index + 1
            while end < n and (src[end].isalnum() or src[end] == "_"):
                end += 1
            name = src[index:end]
            cursor = end
            while cursor < n and src[cursor] in " \t":
                cursor += 1
            if cursor < n and src[cursor] == "=" and name in ("remote_dir", "manifest"):
                line_end = src.find("\n", cursor)
                if line_end < 0:
                    line_end = n
                value = strip_shell_comment(src[cursor + 1 : line_end]).strip()
                parsed = parse_shell_value(name, value)
                if name == "remote_dir":
                    remote = parsed
                else:
                    manifest = parsed
            index = end
            continue
        index += 1
    return remote, manifest


def strip_shell_comment(value):
    # A trailing comment is not part of the path. # inside quotes is.
    out = []
    index = 0
    single = False
    double = False
    while index < len(value):
        char = value[index]
        if single:
            out.append(char)
            if char == "'":
                single = False
            index += 1
            continue
        if double:
            out.append(char)
            if char == "\\" and index + 1 < len(value):
                out.append(value[index + 1])
                index += 2
                continue
            if char == '"':
                double = False
            index += 1
            continue
        if char == "'":
            single = True
        elif char == '"':
            double = True
        elif char == "#":
            break
        out.append(char)
        index += 1
    return "".join(out)


def parse_shell_value(name, value):
    value = value.strip().rstrip(";").strip()
    if name == "remote_dir":
        match = re.fullmatch(
            r'"(?:\$\{dist_dir\}|\$dist_dir)/bin/([^"]+)"|(?:\$\{dist_dir\}|\$dist_dir)/bin/(\S+)',
            value,
        )
    else:
        match = re.fullmatch(
            r'"(?:\$\{remote_dir\}|\$remote_dir)/([^"]+)"|(?:\$\{remote_dir\}|\$remote_dir)/(\S+)',
            value,
        )
    if not match:
        return None
    return match.group(1) or match.group(2)


def const_value(src, masked, spans, const_name):
    values = []
    pattern = r"\bpub\s+const\s+" + re.escape(const_name) + r"\b"
    for match in re.finditer(pattern, masked):
        if covered(spans, match.start(), "linux") is not True:
            continue
        if covered(spans, match.start(), "macos") is not True:
            continue
        semi = masked.find(";", match.end())
        if semi < 0:
            continue
        chunk = src[match.start() : semi]
        literal = re.search(
            r'=\s*(?:r#+"([^"]*)"#+|"([^"]*)"|r"([^"]*)")',
            chunk,
        )
        if not literal:
            values.append(None)
            continue
        values.append(next(group for group in literal.groups() if group is not None))
    live = [value for value in values if value is not None]
    if not live:
        return None
    if any(value != live[0] for value in live):
        return None
    return live[0]


def allowlist_remote_ssh(text):
    found = []
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].rstrip()
        if line == "":
            continue
        if line.startswith("RemoteSsh::"):
            found.append(line)
    return found


# Assertion 1: the wasm workspace installs the relay client from init_app_state.
# A comment, #[cfg(any())], or a different function leaves the browser unable to open ssh.
file_1 = os.path.join(root, "web/crates/zed_web_workspace/src/main.rs")
if require_file(file_1, "[1/6]"):
    text = read_text(file_1)
    if text is not None:
        masked = mask_rust(text)
        spans = cfg_spans(text, masked)
        found = False
        for match in re.finditer(r"\bfn\s+init_app_state\b", masked):
            body_start, body_end = function_body(masked, match.end())
            if live_call(
                masked,
                body_start,
                body_end,
                "remote::set_web_rpc_client(",
                lambda pos: covered(spans, pos, "wasm") is True,
            ):
                found = True
        if found:
            print("PASS [1/6]: web workspace installs remote::set_web_rpc_client")
        else:
            fail(
                "FAIL [1/6]: init_app_state has no remote::set_web_rpc_client( compiled for wasm"
            )

# Assertion 2: the server registers the bundle provider from the install function.
file_2 = os.path.join(root, "crates/zed_web_server/src/ssh_host.rs")
if require_file(file_2, "[2/6]"):
    text = read_text(file_2)
    if text is not None:
        masked = mask_rust(text)
        spans = cfg_spans(text, masked)
        found = False
        for match in re.finditer(r"\bfn\s+install_bundled_remote_server_provider\b", masked):
            body_start, body_end = function_body(masked, match.end())
            if live_call(
                masked,
                body_start,
                body_end,
                "set_bundled_remote_server_provider(",
                lambda pos: covered(spans, pos, "linux") is True
                and covered(spans, pos, "macos") is True,
            ):
                found = True
        if found:
            print("PASS [2/6]: zed_web_server registers set_bundled_remote_server_provider")
        else:
            fail(
                "FAIL [2/6]: install_bundled_remote_server_provider has no live "
                "set_bundled_remote_server_provider( for linux and macos"
            )

# Assertion 3: the import script and the bundle loader name the same directory and manifest.
file_3_rs = os.path.join(root, "crates/zed_web_server/src/remote_server_bundle.rs")
file_3_sh = os.path.join(root, "web/scripts/import-remote-server.sh")
if not os.path.isfile(file_3_rs):
    fail(f"FAIL [3/6]: {file_3_rs} does not exist")
elif not os.path.isfile(file_3_sh):
    fail(f"FAIL [3/6]: {file_3_sh} does not exist")
else:
    rust_text = read_text(file_3_rs)
    shell_text = read_text(file_3_sh)
    if rust_text is not None and shell_text is not None:
        masked = mask_rust(rust_text)
        spans = cfg_spans(rust_text, masked)
        rust_dir = const_value(rust_text, masked, spans, "BUNDLE_DIRECTORY_NAME")
        rust_manifest = const_value(rust_text, masked, spans, "MANIFEST_FILE_NAME")
        shell_dir, shell_manifest = shell_assignments(shell_text)
        if rust_dir is None or rust_manifest is None:
            fail(
                "FAIL [3/6]: no live BUNDLE_DIRECTORY_NAME/MANIFEST_FILE_NAME "
                f"for linux and macos (got {rust_dir!r}, {rust_manifest!r})"
            )
        elif shell_dir is None or shell_manifest is None:
            fail(
                "FAIL [3/6]: no top-level remote_dir/manifest assignment "
                f"(got {shell_dir!r}, {shell_manifest!r})"
            )
        elif rust_dir != shell_dir or rust_manifest != shell_manifest:
            fail(
                "FAIL [3/6]: manifest path mismatch: "
                f"bundle.rs has ({rust_dir!r}, {rust_manifest!r}), "
                f"import script has ({shell_dir!r}, {shell_manifest!r})"
            )
        else:
            print(
                f"PASS [3/6]: bundle constants match import script paths ({rust_dir}/{rust_manifest})"
            )

# Assertion 4: ConnectionPool::connect's wasm arm constructs WebRelayConnection.
file_4 = os.path.join(root, "crates/remote/src/remote_client.rs")
if require_file(file_4, "[4/6]"):
    text = read_text(file_4)
    if text is not None:
        masked = mask_rust(text)
        spans = cfg_spans(text, masked)
        found = False
        for impl_start, impl_end in impl_bodies(masked, "ConnectionPool"):
            for _fn_pos, body_start, body_end in functions_named(
                masked, "connect", (impl_start, impl_end)
            ):
                if live_call(
                    masked,
                    body_start,
                    body_end,
                    "WebRelayConnection",
                    lambda pos: covered(spans, pos, "wasm") is True
                    and covered(spans, pos, "linux") is False,
                ):
                    found = True
        if found:
            print("PASS [4/6]: ConnectionPool::connect has wasm WebRelayConnection branch")
        else:
            fail(
                "FAIL [4/6]: ConnectionPool::connect has no WebRelayConnection "
                "compiled for wasm and excluded from linux"
            )

# Assertion 5: SshRemoteConnection::new exists on desktop and not on wasm.
file_5 = os.path.join(root, "crates/remote/src/transport/ssh.rs")
if require_file(file_5, "[5/6]"):
    text = read_text(file_5)
    if text is not None:
        masked = mask_rust(text)
        spans = cfg_spans(text, masked)
        constructors = []
        for impl_start, impl_end in impl_bodies(masked, "SshRemoteConnection"):
            constructors.extend(functions_named(masked, "new", (impl_start, impl_end)))
        good = True
        if not constructors:
            good = False
        for fn_pos, _body_start, _body_end in constructors:
            # False on wasm and true on every desktop target: cfg(any()) drops the
            # desktop constructor, and a comment leaves it compiled for wasm.
            if covered(spans, fn_pos, "wasm") is not False:
                good = False
            for desktop in ("linux", "macos", "windows"):
                if covered(spans, fn_pos, desktop) is not True:
                    good = False
        if good:
            print(
                'PASS [5/6]: SshRemoteConnection::new is guarded with cfg(not(target_family = "wasm"))'
            )
        else:
            fail(
                'FAIL [5/6]: SshRemoteConnection::new is not cfg-excluded from wasm '
                "while remaining on linux, macos, and windows"
            )

# Assertion 6: no RemoteSsh:: allowlist entry. Comments are not entries.
# An empty or comment-only file has no such entry; a missing file does.
file_6 = os.path.join(root, "web/one-sided-rpc.allowlist")
if require_file(file_6, "[6/6]"):
    text = read_text(file_6)
    if text is not None:
        entries = allowlist_remote_ssh(text)
        if entries:
            rendered = "\n".join(entries)
            fail(f"FAIL [6/6]: {file_6} contains RemoteSsh:: entries:\n{rendered}")
        else:
            print("PASS [6/6]: web/one-sided-rpc.allowlist contains no RemoteSsh:: entries")

if failures:
    print(f"\n{failures} assertion(s) failed", file=sys.stderr)
    sys.exit(1)
PY
printf '\nREMOTE SEAMS OK\n'
