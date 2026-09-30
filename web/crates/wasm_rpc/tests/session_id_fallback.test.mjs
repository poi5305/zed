// Run with: node --test web/crates/wasm_rpc/tests/session_id_fallback.test.mjs
// Loads the inline JS out of wasm_rpc's src/lib.rs (the only copy) and checks that a tab
// still gets its own RPC session when the usual id sources fail.
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

const directory = mkdtempSync(path.join(tmpdir(), "rpc-session-fallback-"));
const rpcModulePath = path.join(directory, "rpc.mjs");
writeFileSync(
    rpcModulePath,
    extractInlineJs(path.join(here, "../src/lib.rs"), "export function zedRpcCreate"),
);

globalThis.self = globalThis;
Object.defineProperty(globalThis, "navigator", { value: { onLine: true }, configurable: true });

const page = { href: "", replaceStateCalls: [] };
const location = {
    get href() {
        return page.href;
    },
    reload() {},
};
Object.defineProperty(globalThis, "location", {
    configurable: true,
    value: location,
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

function loadPage(href) {
    page.href = href;
    page.replaceStateCalls = [];
}

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

function sessionPairSummary(first, second) {
    const sharedDefault = first === "workspace:default" || second === "workspace:default";
    return `same=${first === second} default=${sharedDefault} first=${first} second=${second}`;
}

const pageWithPaths = "http://zed.test/?path=%2Fsrv%2Fmy%20project&path=%2Fsrv%2Fa+b%2Bc";

test("a tab still gets its own session when crypto.getRandomValues throws", { timeout: 2000 }, () => {
    const cryptoDescriptor = Object.getOwnPropertyDescriptor(globalThis, "crypto");
    Object.defineProperty(globalThis, "crypto", {
        configurable: true,
        value: {
            getRandomValues() {
                throw new Error("no crypto");
            },
        },
    });
    try {
        loadPage(pageWithPaths);
        const first = firstRequestSessionId();
        loadPage(pageWithPaths);
        const second = firstRequestSessionId();
        assert.equal(
            sessionPairSummary(first, second),
            `same=false default=false first=${first} second=${second}`,
        );
        assert.equal(new URL(page.href).searchParams.get("workspace_id"), second.slice("workspace:".length));
    } finally {
        if (cryptoDescriptor) {
            Object.defineProperty(globalThis, "crypto", cryptoDescriptor);
        } else {
            delete globalThis.crypto;
        }
    }
});

test("a tab still gets its own session when location.href cannot be parsed", { timeout: 2000 }, () => {
    const locationDescriptor = Object.getOwnPropertyDescriptor(globalThis, "location");
    Object.defineProperty(globalThis, "location", {
        configurable: true,
        value: {
            get href() {
                throw new Error("no location");
            },
            reload() {},
        },
    });
    try {
        const first = firstRequestSessionId();
        const second = firstRequestSessionId();
        assert.equal(
            sessionPairSummary(first, second),
            `same=false default=false first=${first} second=${second}`,
        );
    } finally {
        if (locationDescriptor) {
            Object.defineProperty(globalThis, "location", locationDescriptor);
        }
    }
});
