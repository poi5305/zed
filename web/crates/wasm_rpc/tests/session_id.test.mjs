// Run with: node --test web/crates/wasm_rpc/tests/session_id.test.mjs
// Loads the inline JS out of wasm_rpc's src/lib.rs and zed_web_workspace's src/main.rs (the only
// copies) and checks which RPC session a browser tab ends up in.
import assert from "node:assert/strict";
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const marker = 'inline_js = r#"';

function extractInlineJs(file, containing) {
    const source = readFileSync(file, "utf8");
    let start = source.indexOf(marker);
    while (start !== -1) {
        const end = source.indexOf('"#)]', start);
        assert.ok(end > start, `unterminated inline_js block in ${file}`);
        const block = source.slice(start + marker.length, end);
        if (block.includes(containing)) return block;
        start = source.indexOf(marker, end);
    }
    assert.fail(`no inline_js block with ${containing} in ${file}`);
}

const directory = mkdtempSync(path.join(tmpdir(), "rpc-session-"));
const rpcModulePath = path.join(directory, "rpc.mjs");
writeFileSync(
    rpcModulePath,
    extractInlineJs(path.join(here, "../src/lib.rs"), "export function zedRpcCreate"),
);
const workspaceModulePath = path.join(directory, "workspace.mjs");
writeFileSync(
    workspaceModulePath,
    extractInlineJs(
        path.join(here, "../../zed_web_workspace/src/main.rs"),
        "export function zedOpenWorkspaceInNewTab",
    ),
);

globalThis.self = globalThis;
Object.defineProperty(globalThis, "navigator", { value: { onLine: true }, configurable: true });

const page = { href: "", replaceStateCalls: [] };
Object.defineProperty(globalThis, "location", {
    configurable: true,
    value: {
        get href() {
            return page.href;
        },
        reload() {},
    },
});
globalThis.history = {
    state: null,
    replaceState(state, _title, url) {
        page.replaceStateCalls.push(String(url));
        page.href = new URL(String(url), page.href).href;
    },
};

const sockets = [];
class FakeWebSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;

    constructor(url) {
        this.url = url;
        this.readyState = FakeWebSocket.CONNECTING;
        this.sent = [];
        sockets.push(this);
    }

    send(data) {
        this.sent.push(data);
    }

    close() {
        this.readyState = FakeWebSocket.CLOSING;
    }
}
globalThis.WebSocket = FakeWebSocket;

const { zedRpcCreate } = await import(pathToFileURL(rpcModulePath).href);
const { zedOpenWorkspaceInNewTab } = await import(pathToFileURL(workspaceModulePath).href);

function loadPage(href) {
    page.href = href;
    page.replaceStateCalls = [];
}

// Opens a client, completes the handshake, and returns the session id of its first request.
function firstRequestSessionId() {
    const client = zedRpcCreate("ws://zed.test/rpc", () => {}, () => {}, () => {}, () => {});
    const socket = sockets[sockets.length - 1];
    socket.readyState = FakeWebSocket.OPEN;
    socket.onopen();
    socket.onmessage({
        data: JSON.stringify({ method: "Server::hello", params: { instance_id: "instance-1" } }),
    });
    client.send(JSON.stringify({ id: 1, method: "Fs::metadata", params: {} }));
    client.close();
    assert.equal(socket.sent.length, 1, "the request was not sent after the handshake");
    return JSON.parse(socket.sent[0]).session_id;
}

const pageWithPaths = "http://zed.test/?path=%2Fsrv%2Fmy%20project&path=%2Fsrv%2Fa+b%2Bc";

test("a tab without workspace_id gets its own session and writes it into the URL", () => {
    loadPage(pageWithPaths);
    const sessionId = firstRequestSessionId();

    assert.notEqual(sessionId, "workspace:default", "the tab fell back to the shared session");
    const workspaceId = new URL(page.href).searchParams.get("workspace_id");
    assert.ok(workspaceId, `workspace_id was not written into the URL: ${page.href}`);
    assert.equal(sessionId, `workspace:${workspaceId}`);
    assert.equal(page.replaceStateCalls.length, 1, "expected exactly one replaceState");
});

test("writing workspace_id keeps the existing path parameters byte for byte", () => {
    loadPage(pageWithPaths);
    firstRequestSessionId();

    const query = new URL(page.href).search;
    assert.ok(
        query.startsWith("?path=%2Fsrv%2Fmy%20project&path=%2Fsrv%2Fa+b%2Bc&workspace_id="),
        `the path parameters were re-encoded: ${query}`,
    );
});

test("two new tabs get different sessions", () => {
    loadPage(pageWithPaths);
    const first = firstRequestSessionId();
    loadPage(pageWithPaths);
    const second = firstRequestSessionId();
    assert.notEqual(first, second);
});

test("a reload keeps the session named in the URL and does not touch the URL", () => {
    loadPage(`${pageWithPaths}&workspace_id=0123abcd`);
    assert.equal(firstRequestSessionId(), "workspace:0123abcd");
    assert.deepEqual(page.replaceStateCalls, []);
});

test("a workspace opened in a new tab does not inherit this tab's session", () => {
    loadPage(`${pageWithPaths}&workspace_id=0123abcd`);
    let opened = null;
    globalThis.__zedOpenExternalUrl = url => {
        opened = url;
        return true;
    };
    try {
        assert.equal(zedOpenWorkspaceInNewTab(JSON.stringify(["/srv/other"])), true);
    } finally {
        delete globalThis.__zedOpenExternalUrl;
    }
    const params = new URL(opened).searchParams;
    assert.deepEqual(params.getAll("path"), ["/srv/other"]);
    assert.equal(params.get("workspace_id"), null, `the new tab's URL kept workspace_id: ${opened}`);
});

test("an empty workspace_id is replaced, not followed by a second one", () => {
    loadPage(`${pageWithPaths}&workspace_id=`);
    const sessionId = firstRequestSessionId();
    const workspaceIds = new URL(page.href).searchParams.getAll("workspace_id");
    assert.equal(workspaceIds.length, 1, `expected one workspace_id: ${page.href}`);
    assert.equal(sessionId, `workspace:${workspaceIds[0]}`);
});
