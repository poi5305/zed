// Run with: node --test web/crates/wasm_rpc/tests/byte_channel.test.mjs
// Loads the inline JS out of src/lib.rs (the only copy) and drives it with a fake WebSocket.
import assert from "node:assert/strict";
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const source = readFileSync(path.join(here, "../src/lib.rs"), "utf8");
const start = source.indexOf('inline_js = r#"') + 'inline_js = r#"'.length;
const end = source.indexOf('"#)]', start);
assert.ok(start > 'inline_js = r#"'.length && end > start, "inline_js block not found in lib.rs");
const modulePath = path.join(mkdtempSync(path.join(tmpdir(), "byte-channel-")), "glue.mjs");
writeFileSync(modulePath, source.slice(start, end));

globalThis.self = globalThis;
const sockets = [];

class FakeWebSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;

    constructor(url) {
        this.url = url;
        this.readyState = FakeWebSocket.CONNECTING;
        this.bufferedAmount = 0;
        this.sent = [];
        this.closeCalls = [];
        sockets.push(this);
    }

    send(data) {
        this.sent.push(data);
    }

    close(code, reason) {
        // Browsers only accept 1000 and 3000-4999 from script.
        if (code !== undefined && code !== 1000 && !(code >= 3000 && code <= 4999)) {
            throw new Error(`InvalidAccessError: close code ${code}`);
        }
        this.closeCalls.push({ code, reason });
        this.readyState = FakeWebSocket.CLOSING;
    }

    open() {
        this.readyState = FakeWebSocket.OPEN;
        this.onopen();
    }

    finishClose(code, reason, wasClean) {
        this.readyState = FakeWebSocket.CLOSED;
        this.onclose({ code, reason, wasClean });
    }
}
globalThis.WebSocket = FakeWebSocket;

const { zedRpcOpenBytes } = await import(pathToFileURL(modulePath).href);

function openChannel() {
    const events = { messages: [], closes: [], errors: [] };
    const channel = zedRpcOpenBytes(
        "ws://example.test/remote/channel",
        data => {
            events.messages.push(data);
            return true;
        },
        (code, reason, wasClean) => events.closes.push({ code, reason, wasClean }),
        message => events.errors.push(message),
    );
    return { channel, socket: sockets[sockets.length - 1], events };
}

const MIB = 1024 * 1024;

async function settlesWithin(promise, milliseconds) {
    let timer;
    const timeout = new Promise(resolve => {
        timer = setTimeout(() => resolve("pending"), milliseconds);
    });
    const outcome = await Promise.race([promise.then(value => ({ value })), timeout]);
    clearTimeout(timer);
    return outcome;
}

test("B2: bytes queued before onopen count toward the 8 MiB high-water mark", async () => {
    const { channel, socket } = openChannel();
    channel.send(new Uint8Array(9 * MIB));

    const outcome = await settlesWithin(channel.waitWritable(), 100);
    assert.equal(
        outcome,
        "pending",
        `waitWritable resolved ${JSON.stringify(outcome)} with 9 MiB queued before open; expected it to stay pending`,
    );

    socket.open();
    const afterOpen = await settlesWithin(channel.waitWritable(), 500);
    assert.deepEqual(afterOpen, { value: true }, "writable again once the queue was flushed");
    assert.equal(socket.sent.length, 1);
});

test("B2 guard: a small pre-open queue does not block the writer", async () => {
    const { channel } = openChannel();
    channel.send(new Uint8Array(1 * MIB));
    assert.deepEqual(await settlesWithin(channel.waitWritable(), 500), { value: true });
});

test("B2 guard: one message larger than the mark is still accepted when nothing is queued", async () => {
    const { channel, socket } = openChannel();
    assert.deepEqual(await settlesWithin(channel.waitWritable(), 500), { value: true });
    channel.send(new Uint8Array(64 * MIB));
    socket.open();
    assert.equal(socket.sent.length, 1);
    assert.equal(socket.sent[0].byteLength, 64 * MIB);
});

test("B3: send on a socket the peer is already closing throws instead of losing bytes", () => {
    const { channel, socket } = openChannel();
    socket.open();
    socket.readyState = FakeWebSocket.CLOSING;
    assert.throws(
        () => channel.send(new Uint8Array([1, 2, 3])),
        /closed/,
        "send() on a CLOSING socket returned normally; the bytes were queued and will never be flushed",
    );
});

test("queued bytes are flushed in order on open, then a pending close runs", () => {
    const { channel, socket } = openChannel();
    channel.send(new Uint8Array([1]));
    channel.send(new Uint8Array([2]));
    channel.close(1000, "client closed");
    assert.equal(socket.closeCalls.length, 0);
    socket.open();
    assert.deepEqual(socket.sent.map(chunk => chunk[0]), [1, 2]);
    assert.deepEqual(socket.closeCalls, [{ code: 1000, reason: "client closed" }]);
});

test("a text frame closes the socket (1003 is not allowed from script, so 4003)", () => {
    const { socket, events } = openChannel();
    socket.open();
    socket.onmessage({ data: "hello" });
    assert.equal(events.errors.length, 1);
    assert.deepEqual(socket.closeCalls, [{ code: 4003, reason: "text frames are not supported" }]);
});

test("close reports code, reason and wasClean to the callback", () => {
    const { socket, events } = openChannel();
    socket.open();
    socket.finishClose(1006, "", false);
    assert.deepEqual(events.closes, [{ code: 1006, reason: "", wasClean: false }]);
});

test("a relative URL is resolved against the page, with http(s) mapped to ws(s)", () => {
    const originalLocation = globalThis.location;
    try {
        globalThis.location = { href: "https://zed.example.test/workspace/?path=%2Fsrc" };
        zedRpcOpenBytes("/remote/channel?channel_id=ch-1&token=abc", () => true, () => {}, () => {});
        assert.equal(
            sockets[sockets.length - 1].url,
            "wss://zed.example.test/remote/channel?channel_id=ch-1&token=abc",
        );
        globalThis.location = { href: "http://127.0.0.1:8080/" };
        zedRpcOpenBytes("/remote/channel?channel_id=ch-2&token=def", () => true, () => {}, () => {});
        assert.equal(
            sockets[sockets.length - 1].url,
            "ws://127.0.0.1:8080/remote/channel?channel_id=ch-2&token=def",
        );
    } finally {
        globalThis.location = originalLocation;
    }
});

test("an absolute ws URL is used as given", () => {
    const { socket } = openChannel();
    assert.equal(socket.url, "ws://example.test/remote/channel");
});
