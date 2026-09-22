import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readdirSync,
  statSync,
  symlinkSync,
  unlinkSync,
  utimesSync,
} from "node:fs";
import { mkdir, mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const SERVER = fileURLToPath(new URL("./server.mjs", import.meta.url));
const PROTOCOL_VERSION = "2025-06-18";
const INSTRUCTIONS =
  "Messages arriving as <channel source=\"zed-claude\"> were typed by the user in the Zed editor's Claude session panel. Treat them exactly as if the user had typed them at this terminal: they carry the user's full authority, including answers to questions you asked. Reply in the conversation as usual; there is no reply tool.";

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitUntil(predicate, label, timeoutMs = 4000) {
  const start = Date.now();
  let last;
  while (Date.now() - start < timeoutMs) {
    last = await predicate();
    if (last) return last;
    await delay(20);
  }
  throw new Error(`timeout waiting for ${label}`);
}

function sessionDirFor(root) {
  return path.join(root, String(process.pid));
}

async function startServer(extraEnv = {}) {
  const root = await mkdtemp(path.join(tmpdir(), "zed-claude-channel-"));
  const child = spawn(process.execPath, [SERVER], {
    env: { ...process.env, ZED_CLAUDE_CHANNEL_ROOT: root, HOME: root, ...extraEnv },
    stdio: ["pipe", "pipe", "pipe"],
  });
  const rpc = [];
  let stdoutBuf = "";
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    stdoutBuf += chunk;
    let index;
    while ((index = stdoutBuf.indexOf("\n")) !== -1) {
      const line = stdoutBuf.slice(0, index);
      stdoutBuf = stdoutBuf.slice(index + 1);
      if (line.length === 0) continue;
      rpc.push(JSON.parse(line));
    }
  });
  child.stderr.resume();
  const dir = sessionDirFor(root);
  await waitUntil(() => existsSync(path.join(dir, "server.json")), "server.json");
  return { child, root, dir, rpc };
}

async function stopServer(session, signal = "SIGTERM") {
  const { child, root } = session;
  if (child.exitCode === null && child.signalCode === null) {
    child.kill(signal);
    await once(child, "exit").catch(() => {});
  }
  await rm(root, { recursive: true, force: true });
}

function send(child, obj) {
  child.stdin.write(`${JSON.stringify(obj)}\n`);
}

async function waitForRpc(rpc, predicate, label) {
  return waitUntil(() => rpc.find(predicate), label);
}

async function initialize(child, rpc, id = 1) {
  send(child, {
    jsonrpc: "2.0",
    id,
    method: "initialize",
    params: {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "test-client", version: "1.2.3" },
    },
  });
  return waitForRpc(
    rpc,
    (msg) => msg.id === id && msg.result,
    "initialize result",
  );
}

function notifyInitialized(child) {
  send(child, { jsonrpc: "2.0", method: "notifications/initialized" });
}

async function handshake(child, rpc) {
  const response = await initialize(child, rpc);
  notifyInitialized(child);
  return response;
}

async function dropOutbox(dir, name, body) {
  const outbox = path.join(dir, "outbox");
  await mkdir(outbox, { recursive: true });
  const dest = path.join(outbox, name);
  const tmpPath = path.join(outbox, `${name}.part.tmp`);
  const payload = typeof body === "string" ? body : JSON.stringify(body);
  await writeFile(tmpPath, payload);
  await rename(tmpPath, dest);
}

async function readInbox(dir) {
  try {
    const text = await readFile(path.join(dir, "inbox.jsonl"), "utf8");
    return text
      .split("\n")
      .filter((line) => line.length > 0)
      .map((line) => JSON.parse(line));
  } catch {
    return [];
  }
}

test("initialize advertises channel capabilities and exact instructions", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    const response = await initialize(session.child, session.rpc);
    const result = response.result;
    assert.equal(result.protocolVersion, PROTOCOL_VERSION);
    assert.deepEqual(result.capabilities.experimental["claude/channel"], {});
    assert.deepEqual(result.capabilities.experimental["claude/channel/permission"], {});
    assert.equal("tools" in result.capabilities, false);
    assert.equal(result.instructions, INSTRUCTIONS);
    assert.deepEqual(result.serverInfo, { name: "zed-claude", version: "0.1.0" });
  } finally {
    await stopServer(session);
  }
});

test("ping, tools/list, and unknown request", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await initialize(session.child, session.rpc, 1);
    send(session.child, { jsonrpc: "2.0", id: 2, method: "ping" });
    const ping = await waitForRpc(session.rpc, (msg) => msg.id === 2, "ping");
    assert.deepEqual(ping.result, {});

    send(session.child, { jsonrpc: "2.0", id: 3, method: "tools/list" });
    const tools = await waitForRpc(session.rpc, (msg) => msg.id === 3, "tools/list");
    assert.deepEqual(tools.result, { tools: [] });

    send(session.child, { jsonrpc: "2.0", id: 4, method: "no/such/method" });
    const unknown = await waitForRpc(session.rpc, (msg) => msg.id === 4, "unknown method");
    assert.equal(unknown.error.code, -32601);
  } finally {
    await stopServer(session);
  }
});

test("outbox message is buffered until notifications/initialized", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await dropOutbox(session.dir, "0001.json", {
      kind: "message",
      content: "hello from zed",
      meta: { from: "panel" },
    });
    await initialize(session.child, session.rpc);
    await delay(400);
    assert.equal(
      session.rpc.filter((msg) => msg.method === "notifications/claude/channel").length,
      0,
    );
    notifyInitialized(session.child);
    const channel = await waitForRpc(
      session.rpc,
      (msg) => msg.method === "notifications/claude/channel",
      "channel notification",
    );
    assert.equal(channel.params.content, "hello from zed");
    assert.equal(channel.params.meta.from, "zed");
    assert.equal(typeof channel.params.meta.sent_at_ms, "string");
    assert.match(channel.params.meta.sent_at_ms, /^\d+$/);
    await waitUntil(() => !existsSync(path.join(session.dir, "outbox", "0001.json")), "outbox delete");
    const sent = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "message_sent"),
      "message_sent",
    );
    assert.equal(sent.outbox_file, "0001.json");
    assert.equal(sent.content_chars, "hello from zed".length);
  } finally {
    await stopServer(session);
  }
});

test("meta sanitising drops invalid keys and stringifies values", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    await dropOutbox(session.dir, "0002.json", {
      kind: "message",
      content: "sanitise me",
      meta: { "bad-key": "x", ok_key: 1 },
    });
    const channel = await waitForRpc(
      session.rpc,
      (msg) => msg.method === "notifications/claude/channel",
      "channel notification",
    );
    assert.equal(channel.params.meta.ok_key, "1");
    assert.equal("bad-key" in channel.params.meta, false);
    assert.equal(channel.params.meta.from, "zed");
    assert.equal(typeof channel.params.meta.sent_at_ms, "string");
    assert.deepEqual(Object.keys(channel.params.meta).sort(), ["from", "ok_key", "sent_at_ms"]);
  } finally {
    await stopServer(session);
  }
});

test("permission_request relay and single verdict", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    send(session.child, {
      jsonrpc: "2.0",
      method: "notifications/claude/channel/permission_request",
      params: {
        request_id: "abcde",
        tool_name: "Bash",
        description: "run ls",
        input_preview: "ls -la",
      },
    });
    const requestLine = await waitUntil(
      async () =>
        (await readInbox(session.dir)).find((line) => line.kind === "permission_request"),
      "permission_request inbox",
    );
    assert.equal(requestLine.request_id, "abcde");
    assert.equal(requestLine.tool_name, "Bash");
    assert.equal(requestLine.description, "run ls");
    assert.equal(requestLine.input_preview, "ls -la");

    await dropOutbox(session.dir, "0003.json", {
      kind: "permission",
      request_id: "abcde",
      behavior: "deny",
    });
    const verdict = await waitForRpc(
      session.rpc,
      (msg) => msg.method === "notifications/claude/channel/permission",
      "permission notification",
    );
    assert.equal(verdict.params.request_id, "abcde");
    assert.equal(verdict.params.behavior, "deny");
    await waitUntil(
      async () =>
        (await readInbox(session.dir)).find((line) => line.kind === "permission_answered"),
      "permission_answered",
    );

    const permissionCount = session.rpc.filter(
      (msg) => msg.method === "notifications/claude/channel/permission",
    ).length;
    await dropOutbox(session.dir, "0004.json", {
      kind: "permission",
      request_id: "abcde",
      behavior: "allow",
    });
    const errorLine = await waitUntil(
      async () =>
        (await readInbox(session.dir)).find(
          (line) => line.kind === "error" && line.outbox_file === "0004.json",
        ),
      "second verdict error",
    );
    assert.equal(typeof errorLine.reason, "string");
    await delay(300);
    assert.equal(
      session.rpc.filter((msg) => msg.method === "notifications/claude/channel/permission")
        .length,
      permissionCount,
    );
  } finally {
    await stopServer(session);
  }
});

test("unknown request_id verdict is rejected without stdout notification", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    await dropOutbox(session.dir, "0005.json", {
      kind: "permission",
      request_id: "missing-id",
      behavior: "allow",
    });
    await waitUntil(
      async () =>
        (await readInbox(session.dir)).find(
          (line) => line.kind === "error" && line.outbox_file === "0005.json",
        ),
      "unknown request_id error",
    );
    await delay(300);
    assert.equal(
      session.rpc.filter((msg) => msg.method === "notifications/claude/channel/permission")
        .length,
      0,
    );
  } finally {
    await stopServer(session);
  }
});

test("invalid JSON outbox file is renamed .bad and server keeps running", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    await dropOutbox(session.dir, "aaaa.json", "{not json");
    await dropOutbox(session.dir, "bbbb.json", {
      kind: "message",
      content: "still works",
    });
    await waitUntil(
      () => existsSync(path.join(session.dir, "outbox", "aaaa.json.bad")),
      "renamed .bad",
    );
    const channel = await waitForRpc(
      session.rpc,
      (msg) =>
        msg.method === "notifications/claude/channel" &&
        msg.params.content === "still works",
      "following valid message",
    );
    assert.equal(channel.params.meta.from, "zed");
    assert.equal(existsSync(path.join(session.dir, "outbox", "aaaa.json")), false);
  } finally {
    await stopServer(session);
  }
});

test("tmp outbox files are ignored", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    const tmpPath = path.join(session.dir, "outbox", "held.json.tmp");
    await writeFile(
      tmpPath,
      JSON.stringify({ kind: "message", content: "should not send yet" }),
    );
    await delay(500);
    assert.equal(
      session.rpc.filter((msg) => msg.method === "notifications/claude/channel").length,
      0,
    );
    assert.equal(existsSync(tmpPath), true);
    await rename(tmpPath, path.join(session.dir, "outbox", "held.json"));
    const channel = await waitForRpc(
      session.rpc,
      (msg) => msg.method === "notifications/claude/channel",
      "message after rename",
    );
    assert.equal(channel.params.content, "should not send yet");
  } finally {
    await stopServer(session);
  }
});

test("two outbox files arrive in lexical order", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await dropOutbox(session.dir, "2.json", { kind: "message", content: "second" });
    await dropOutbox(session.dir, "1.json", { kind: "message", content: "first" });
    await handshake(session.child, session.rpc);
    await waitUntil(
      () =>
        session.rpc.filter((msg) => msg.method === "notifications/claude/channel").length >= 2,
      "both channel messages",
    );
    const sent = session.rpc.filter((msg) => msg.method === "notifications/claude/channel");
    assert.equal(sent[0].params.content, "first");
    assert.equal(sent[1].params.content, "second");
  } finally {
    await stopServer(session);
  }
});

test("stdin EOF writes closed, removes server.json, exits 0", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    session.child.stdin.end();
    const [code] = await once(session.child, "exit");
    assert.equal(code, 0);
    assert.equal(existsSync(path.join(session.dir, "server.json")), false);
    const closed = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "closed"),
      "closed inbox",
    );
    assert.equal(closed.reason, "stdin_end");
  } finally {
    await stopServer(session);
  }
});

test("server.json exists after start with claude_pid equal to the test pid", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    const raw = await readFile(path.join(session.dir, "server.json"), "utf8");
    const info = JSON.parse(raw);
    assert.equal(info.claude_pid, process.pid);
    assert.equal(typeof info.pid, "number");
    assert.equal(info.protocol, 1);
    assert.equal(typeof info.started_at_ms, "number");
    assert.equal(typeof info.heartbeat_at_ms, "number");
  } finally {
    await stopServer(session);
  }
});

// --- review round 1 regression tests -------------------------------------
// One test per fixed finding (F01..F09). Each one must fail against the
// reviewed snapshot for the reason named in the assertion message.

function countChannel(rpc, content) {
  return rpc.filter(
    (msg) => msg.method === "notifications/claude/channel" && msg.params.content === content,
  ).length;
}

function inboxErrors(lines, name) {
  return lines.filter((line) => line.kind === "error" && line.outbox_file === name);
}

async function readServerLog(dir) {
  try {
    return await readFile(path.join(dir, "server.log"), "utf8");
  } catch {
    return "";
  }
}

test("F01 an outbox file that cannot be deleted is sent exactly once", { timeout: 5000 }, async (t) => {
  if (typeof process.getuid === "function" && process.getuid() === 0) {
    t.skip("running as root: a read-only directory would not deny unlink");
    return;
  }
  const session = await startServer();
  const outbox = path.join(session.dir, "outbox");
  try {
    // Hold the poll back so the file is still on disk when the directory
    // becomes read-only, then let the flush run against the locked directory.
    await initialize(session.child, session.rpc);
    await dropOutbox(session.dir, "0001.json", { kind: "message", content: "run rm -rf /" });
    chmodSync(outbox, 0o500);
    notifyInitialized(session.child);
    await delay(1000);
    const sends = countChannel(session.rpc, "run rm -rf /");
    assert.equal(
      sends,
      1,
      `undeletable outbox file was re-sent to the model: actual ${sends} notifications/claude/channel over 5 polls, expected exactly 1`,
    );
    const errors = inboxErrors(await readInbox(session.dir), "0001.json");
    assert.ok(
      errors.length <= 1,
      `inbox.jsonl was spammed for one stuck file: actual ${errors.length} error lines, expected at most 1`,
    );
  } finally {
    chmodSync(outbox, 0o700);
    await stopServer(session);
  }
});

test("F02 an outbox entry that is not a readable file is quarantined once", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    mkdirSync(path.join(session.dir, "outbox", "0001.json"));
    await dropOutbox(session.dir, "0002.json", { kind: "message", content: "after the stuck entry" });
    await delay(1000);
    const quarantined = existsSync(path.join(session.dir, "outbox", "0001.json.bad"));
    assert.equal(
      quarantined,
      true,
      `unreadable outbox entry was never quarantined: actual outbox contents ${JSON.stringify(
        await readdirNames(session.dir),
      )}, expected it to contain "0001.json.bad"`,
    );
    const errors = inboxErrors(await readInbox(session.dir), "0001.json");
    assert.equal(
      errors.length,
      1,
      `stuck entry was retried on every poll: actual ${errors.length} error lines for 0001.json, expected exactly 1`,
    );
    assert.equal(
      countChannel(session.rpc, "after the stuck entry"),
      1,
      "a valid message after the stuck entry must still be delivered",
    );
  } finally {
    await stopServer(session);
  }
});

test("F03 an oversized outbox file is rejected and a large legal message still flows", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    const huge = "H".repeat(5 * 1024 * 1024);
    await dropOutbox(session.dir, "0001.json", { kind: "message", content: huge });
    await delay(700);
    const errors = inboxErrors(await readInbox(session.dir), "0001.json");
    assert.equal(
      errors.length,
      1,
      `5 MiB outbox file was not rejected: actual error lines for 0001.json ${JSON.stringify(
        errors,
      )}, expected exactly one with reason "too_large"`,
    );
    assert.equal(errors[0]?.reason, "too_large", `actual reason ${errors[0]?.reason}, expected "too_large"`);
    assert.equal(
      countChannel(session.rpc, huge),
      0,
      "a 5 MiB outbox file must not be pushed to the model as one stdout line",
    );

    // Boundary proof: the new size limit must not reject a message a user
    // could plausibly paste into the Zed panel.
    const legal = "L".repeat(512 * 1024);
    await dropOutbox(session.dir, "0002.json", { kind: "message", content: legal });
    await waitUntil(() => countChannel(session.rpc, legal) === 1, "512 KiB message");
    const sent = (await readInbox(session.dir)).find(
      (line) => line.kind === "message_sent" && line.outbox_file === "0002.json",
    );
    assert.equal(
      sent?.content_chars,
      legal.length,
      `512 KiB message was altered: actual content_chars ${sent?.content_chars}, expected ${legal.length}`,
    );
  } finally {
    await stopServer(session);
  }
});

test("F04 open permission requests are bounded and the oldest fail closed", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    const total = 600;
    for (let index = 0; index < total; index += 1) {
      send(session.child, {
        jsonrpc: "2.0",
        method: "notifications/claude/channel/permission_request",
        params: {
          request_id: `id-${String(index).padStart(3, "0")}`,
          tool_name: "Bash",
          description: "d",
          input_preview: "p",
        },
      });
    }
    await waitUntil(
      async () =>
        (await readInbox(session.dir)).filter((line) => line.kind === "permission_request").length >=
        total,
      "all permission_request lines",
    );

    await dropOutbox(session.dir, "9000.json", {
      kind: "permission",
      request_id: "id-000",
      behavior: "allow",
    });
    await delay(700);
    const stale = session.rpc.filter(
      (msg) =>
        msg.method === "notifications/claude/channel/permission" &&
        msg.params.request_id === "id-000",
    );
    assert.equal(
      stale.length,
      0,
      `verdict for an evicted request was relayed: actual ${JSON.stringify(
        stale,
      )}, expected no notification because only the newest requests may stay open`,
    );
    const errors = inboxErrors(await readInbox(session.dir), "9000.json");
    assert.equal(
      errors[0]?.reason,
      "unknown_request_id",
      `actual reason ${errors[0]?.reason}, expected "unknown_request_id" for an evicted request`,
    );

    // Boundary proof: eviction must never touch a recent request.
    await dropOutbox(session.dir, "9001.json", {
      kind: "permission",
      request_id: `id-${String(total - 1).padStart(3, "0")}`,
      behavior: "deny",
    });
    const fresh = await waitForRpc(
      session.rpc,
      (msg) =>
        msg.method === "notifications/claude/channel/permission" &&
        msg.params.request_id === "id-599",
      "verdict for the newest request",
    );
    assert.equal(fresh.params.behavior, "deny");
  } finally {
    await stopServer(session);
  }
});

test("F05 a broken stdout pipe does not kill the server or strand server.json", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    await delay(100);
    session.child.stdout.destroy();
    await dropOutbox(session.dir, "0001.json", { kind: "message", content: "into a dead pipe" });
    await delay(700);
    assert.equal(
      session.child.exitCode,
      null,
      `server died on a broken stdout pipe: actual exit code ${session.child.exitCode}, expected null (still running)`,
    );
    const sent = (await readInbox(session.dir)).find((line) => line.kind === "message_sent");
    assert.ok(sent, "the server must keep serving the file protocol after a stdout write error");

    session.child.kill("SIGTERM");
    const [code] = await once(session.child, "exit");
    assert.equal(code, 0);
    const closed = (await readInbox(session.dir)).find((line) => line.kind === "closed");
    assert.equal(
      closed?.reason,
      "signal",
      `no clean shutdown record after a broken pipe: actual ${JSON.stringify(
        closed,
      )}, expected {"kind":"closed","reason":"signal"}`,
    );
    assert.equal(existsSync(path.join(session.dir, "server.json")), false);
  } finally {
    await stopServer(session);
  }
});

test("F06 the heartbeat keeps ticking and survives a vanished session directory", { timeout: 5000 }, async () => {
  const session = await startServer({ ZED_CLAUDE_CHANNEL_HEARTBEAT_MS: "100" });
  try {
    await handshake(session.child, session.rpc);
    const first = JSON.parse(await readFile(path.join(session.dir, "server.json"), "utf8"));
    let latest = first;
    await waitUntil(async () => {
      latest = JSON.parse(await readFile(path.join(session.dir, "server.json"), "utf8"));
      return latest.heartbeat_at_ms > first.heartbeat_at_ms;
    }, "heartbeat to advance", 1000).catch(() => {});
    assert.ok(
      latest.heartbeat_at_ms > first.heartbeat_at_ms,
      `heartbeat did not advance within 1000 ms: actual heartbeat_at_ms ${latest.heartbeat_at_ms}, expected greater than ${first.heartbeat_at_ms}`,
    );

    await rm(session.dir, { recursive: true, force: true });
    await delay(400);
    assert.equal(
      session.child.exitCode,
      null,
      `server died when its session directory was removed: actual exit code ${session.child.exitCode}, expected null (still running)`,
    );
    send(session.child, { jsonrpc: "2.0", id: 42, method: "ping" });
    const pong = await waitForRpc(session.rpc, (msg) => msg.id === 42, "ping after the failed heartbeat");
    assert.deepEqual(pong.result, {});
  } finally {
    await stopServer(session);
  }
});

test("F07 the session directory is private to its owner", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    const dirMode = statSync(session.dir).mode & 0o777;
    const outboxMode = statSync(path.join(session.dir, "outbox")).mode & 0o777;
    assert.equal(
      dirMode.toString(8),
      "700",
      `session directory is exposed to other local users: actual mode ${dirMode.toString(
        8,
      )}, expected 700`,
    );
    assert.equal(
      outboxMode.toString(8),
      "700",
      `outbox directory is exposed to other local users: actual mode ${outboxMode.toString(
        8,
      )}, expected 700`,
    );

    // Boundary proof: 0700 must not lock out the owning process itself.
    await handshake(session.child, session.rpc);
    await dropOutbox(session.dir, "0001.json", { kind: "message", content: "still mine" });
    await waitUntil(() => countChannel(session.rpc, "still mine") === 1, "message through a 0700 dir");
  } finally {
    await stopServer(session);
  }
});

test("F08 an overlong stdin line is dropped, recorded, and the stream resyncs", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);

    // Boundary proof first: a legitimately large line must survive intact.
    const preview = "P".repeat(256 * 1024);
    send(session.child, {
      jsonrpc: "2.0",
      method: "notifications/claude/channel/permission_request",
      params: { request_id: "big", tool_name: "Bash", description: "d", input_preview: preview },
    });
    const big = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.request_id === "big"),
      "256 KiB permission_request",
    );
    assert.equal(
      big.input_preview.length,
      preview.length,
      `a 256 KiB line was truncated: actual input_preview length ${big.input_preview.length}, expected ${preview.length}`,
    );

    session.child.stdin.write("x".repeat(9 * 1024 * 1024));
    await delay(300);
    session.child.stdin.write('\n{"jsonrpc":"2.0","id":77,"method":"ping"}\n');
    const pong = await waitForRpc(session.rpc, (msg) => msg.id === 77, "ping after the overlong line");
    assert.deepEqual(pong.result, {});
    const log = await readServerLog(session.dir);
    assert.match(
      log,
      /stdin line exceeded/,
      `no record that an overlong stdin line was bounded and dropped; actual server.log tail: ${JSON.stringify(
        log.slice(-400),
      )}`,
    );
  } finally {
    await stopServer(session);
  }
});

test("F09 a permission_request carrying a null id is still a notification", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    await handshake(session.child, session.rpc);
    session.child.stdin.write(
      `${JSON.stringify({
        jsonrpc: "2.0",
        id: null,
        method: "notifications/claude/channel/permission_request",
        params: { request_id: "nullid", tool_name: "Bash", description: "d", input_preview: "p" },
      })}\n`,
    );
    await delay(400);
    const lines = await readInbox(session.dir);
    const relayed = lines.find((line) => line.kind === "permission_request" && line.request_id === "nullid");
    assert.ok(
      relayed,
      `permission relay with "id": null was dropped: actual inbox kinds ${JSON.stringify(
        lines.map((line) => line.kind),
      )}, expected a permission_request line for "nullid"`,
    );
    const errors = session.rpc.filter((msg) => msg.error);
    assert.equal(
      errors.length,
      0,
      `a notification was answered with a JSON-RPC error: actual ${JSON.stringify(errors)}, expected none`,
    );
  } finally {
    await stopServer(session);
  }
});

async function readdirNames(dir) {
  const { readdir } = await import("node:fs/promises");
  try {
    return await readdir(path.join(dir, "outbox"));
  } catch {
    return [];
  }
}

// Boundary proof for the F01 guard (not a finding): parking a name must not
// blacklist it forever — once the stuck file is gone, Zed may reuse the name.
test("F01 boundary a parked outbox name is usable again once the file is gone", { timeout: 5000 }, async (t) => {
  if (typeof process.getuid === "function" && process.getuid() === 0) {
    t.skip("running as root: a read-only directory would not deny unlink");
    return;
  }
  const session = await startServer();
  const outbox = path.join(session.dir, "outbox");
  try {
    await initialize(session.child, session.rpc);
    await dropOutbox(session.dir, "0001.json", { kind: "message", content: "parked" });
    chmodSync(outbox, 0o500);
    notifyInitialized(session.child);
    await waitUntil(() => countChannel(session.rpc, "parked") === 1, "first send");
    await delay(400);
    assert.equal(countChannel(session.rpc, "parked"), 1);

    chmodSync(outbox, 0o700);
    await rm(path.join(outbox, "0001.json"), { force: true });
    await delay(400);
    await dropOutbox(session.dir, "0001.json", { kind: "message", content: "reused name" });
    await waitUntil(
      () => countChannel(session.rpc, "reused name") === 1,
      "message under a previously parked name",
    );
  } finally {
    chmodSync(outbox, 0o700);
    await stopServer(session);
  }
});

// --- review round 2 regression tests -------------------------------------
// One test per fixed finding. F13: parking must key the inode, not just the
// basename, so Zed's tmp+rename onto a parked name is still delivered.
// Legal input this guard must not reject: a replacement file at a parked
// name (new inode, same basename) without a poll seeing the name absent.
// Same-inode parked files must still be sent only once (asserted below; F01).

test("F13 a replacement at a parked outbox name is sent", { timeout: 5000 }, async (t) => {
  if (typeof process.getuid === "function" && process.getuid() === 0) {
    t.skip("running as root: a read-only directory would not deny unlink");
    return;
  }
  const session = await startServer();
  const outbox = path.join(session.dir, "outbox");
  try {
    await initialize(session.child, session.rpc);
    await dropOutbox(session.dir, "0001.json", { kind: "message", content: "parked" });
    chmodSync(outbox, 0o500);
    notifyInitialized(session.child);
    await waitUntil(() => countChannel(session.rpc, "parked") === 1, "first send");

    chmodSync(outbox, 0o700);
    // No delay: tmp+rename onto the still-present parked name, so readdir never
    // sees a gap. Prune-by-absence must not be required for the replacement.
    await dropOutbox(session.dir, "0001.json", { kind: "message", content: "replaced without a gap" });
    await delay(800);
    const sends = countChannel(session.rpc, "replaced without a gap");
    assert.equal(
      sends,
      1,
      `parked outbox name starved a replacement file at the same name: actual ${sends} notifications/claude/channel, expected exactly 1`,
    );
    assert.equal(
      countChannel(session.rpc, "parked"),
      1,
      `replacement handling re-sent the parked inode: actual ${countChannel(session.rpc, "parked")} notifications, expected exactly 1`,
    );
  } finally {
    chmodSync(outbox, 0o700);
    await stopServer(session);
  }
});

// --- WP3c interrupt (SIGINT to the parent / Claude process) ----------------
// The test process is this server's parent, so a real SIGINT would also hit
// `node --test`. Displace its listeners for the duration of each test.

function trapSigint() {
  let count = 0;
  const handler = () => {
    count += 1;
  };
  const previous = process.listeners("SIGINT").slice();
  process.removeAllListeners("SIGINT");
  process.on("SIGINT", handler);
  return {
    get count() {
      return count;
    },
    restore() {
      process.removeListener("SIGINT", handler);
      for (const listener of previous) {
        process.on("SIGINT", listener);
      }
    },
  };
}

test("interrupt delivers exactly one SIGINT and one interrupted inbox line", { timeout: 5000 }, async () => {
  const session = await startServer();
  const sigint = trapSigint();
  try {
    await handshake(session.child, session.rpc);
    await dropOutbox(session.dir, "int-1.json", { kind: "interrupt", reason: "stop-turn" });
    const interrupted = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "interrupted"),
      "interrupted inbox",
    );
    await waitUntil(() => sigint.count >= 1, "SIGINT from interrupt");
    assert.equal(
      sigint.count,
      1,
      `actual SIGINT count ${sigint.count}, expected 1`,
    );
    assert.equal(
      interrupted.claude_pid,
      process.pid,
      `actual claude_pid ${interrupted.claude_pid}, expected ${process.pid}`,
    );
    assert.equal(
      interrupted.reason,
      "stop-turn",
      `actual reason ${JSON.stringify(interrupted.reason)}, expected "stop-turn"`,
    );
    assert.equal(
      typeof interrupted.at_ms,
      "number",
      `actual at_ms ${JSON.stringify(interrupted.at_ms)}, expected a number`,
    );
    const interruptedLines = (await readInbox(session.dir)).filter((line) => line.kind === "interrupted");
    assert.equal(
      interruptedLines.length,
      1,
      `actual interrupted inbox lines ${interruptedLines.length}, expected 1`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
  }
});

test("a second interrupt within 3s is interrupt_throttled and does not SIGINT", { timeout: 5000 }, async () => {
  const session = await startServer();
  const sigint = trapSigint();
  try {
    await initialize(session.child, session.rpc);
    await dropOutbox(session.dir, "int-a.json", { kind: "interrupt" });
    await dropOutbox(session.dir, "int-b.json", { kind: "interrupt" });
    notifyInitialized(session.child);
    const interrupted = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "interrupted"),
      "first interrupted inbox",
    );
    const throttled = await waitUntil(
      async () =>
        (await readInbox(session.dir)).find(
          (line) => line.kind === "error" && line.outbox_file === "int-b.json",
        ),
      "throttled second interrupt",
    );
    await waitUntil(() => sigint.count >= 1, "SIGINT from first interrupt");
    await delay(300);
    assert.equal(
      sigint.count,
      1,
      `actual SIGINT count ${sigint.count}, expected 1 (second interrupt must not signal)`,
    );
    assert.equal(
      throttled.reason,
      "interrupt_throttled",
      `actual reason ${JSON.stringify(throttled.reason)}, expected "interrupt_throttled"`,
    );
    assert.equal(
      interrupted.reason,
      null,
      `actual first interrupt reason ${JSON.stringify(interrupted.reason)}, expected null`,
    );
    const interruptedLines = (await readInbox(session.dir)).filter((line) => line.kind === "interrupted");
    assert.equal(
      interruptedLines.length,
      1,
      `actual interrupted inbox lines ${interruptedLines.length}, expected 1`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
  }
});

test("an interrupt whose mtime is 20s old is interrupt_stale and does not SIGINT", { timeout: 5000 }, async () => {
  const session = await startServer();
  const sigint = trapSigint();
  try {
    await initialize(session.child, session.rpc);
    await dropOutbox(session.dir, "int-stale.json", { kind: "interrupt", reason: "too late" });
    const filePath = path.join(session.dir, "outbox", "int-stale.json");
    const past = new Date(Date.now() - 20_000);
    utimesSync(filePath, past, past);
    notifyInitialized(session.child);
    const stale = await waitUntil(
      async () =>
        (await readInbox(session.dir)).find(
          (line) => line.kind === "error" && line.outbox_file === "int-stale.json",
        ),
      "interrupt_stale",
    );
    await delay(300);
    assert.equal(
      stale.reason,
      "interrupt_stale",
      `actual reason ${JSON.stringify(stale.reason)}, expected "interrupt_stale"`,
    );
    assert.equal(
      sigint.count,
      0,
      `actual SIGINT count ${sigint.count}, expected 0 for a stale interrupt`,
    );
    const interruptedLines = (await readInbox(session.dir)).filter((line) => line.kind === "interrupted");
    assert.equal(
      interruptedLines.length,
      0,
      `actual interrupted inbox lines ${interruptedLines.length}, expected 0`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
  }
});

test("server.json lists message, permission, and interrupt features", { timeout: 5000 }, async () => {
  const session = await startServer();
  try {
    const raw = await readFile(path.join(session.dir, "server.json"), "utf8");
    const info = JSON.parse(raw);
    const expected = ["message", "permission", "interrupt"];
    assert.deepEqual(
      info.features,
      expected,
      `actual features ${JSON.stringify(info.features)}, expected ${JSON.stringify(expected)}`,
    );
  } finally {
    await stopServer(session);
  }
});

// --- WP3c review round 1 regression tests ----------------------------------
// I01..I04, one test per fixed finding, each failing against the reviewed
// snapshot for the reason named in its assertion message. They key on
// inbox.jsonl rather than on a SIGINT count wherever the two signals could
// land in the same tick, because POSIX coalesces non-realtime signals (I06).

// Preloaded into the server process with NODE_OPTIONS=--require so a clock
// step and a reparenting can be simulated from outside. Both are armed by a
// marker file, never by elapsed time, so the ordering is deterministic. The
// server reads process.ppid at module load, long before the marker exists,
// so the session directory is still keyed to the real parent.
const PRELOAD_SOURCE = `
"use strict";
const fs = require("node:fs");
const realNow = Date.now;
const realPpid = process.ppid;
const marker = process.env.ZED_TEST_PRELOAD_MARKER;
const clockShiftMs = Number(process.env.ZED_TEST_CLOCK_SHIFT_MS || 0);
const fakePpid = Number(process.env.ZED_TEST_FAKE_PPID || 0);
let armed = false;
const watch = setInterval(() => {
  if (!armed && marker && fs.existsSync(marker)) armed = true;
}, 25);
watch.unref();
Date.now = function now() {
  const value = realNow();
  return armed ? value + clockShiftMs : value;
};
Object.defineProperty(process, "ppid", {
  configurable: true,
  enumerable: true,
  get() {
    return armed && fakePpid ? fakePpid : realPpid;
  },
});
`;

const DECOY_SOURCE = `
const fs = require("node:fs");
process.on("SIGINT", () => {
  try {
    fs.writeFileSync(process.env.ZED_TEST_DECOY_MARKER, "signalled");
  } catch {}
  process.exit(0);
});
setInterval(() => {}, 1000);
console.log("ready");
`;

async function withPreload() {
  const dir = await mkdtemp(path.join(tmpdir(), "zed-claude-preload-"));
  const file = path.join(dir, "zed-test-preload.cjs");
  const marker = path.join(dir, "arm");
  await writeFile(file, PRELOAD_SOURCE);
  return {
    dir,
    marker,
    env(extra = {}) {
      return {
        NODE_OPTIONS: `--require "${file}"`,
        ZED_TEST_PRELOAD_MARKER: marker,
        ...extra,
      };
    },
    arm() {
      return writeFile(marker, "1");
    },
    cleanup() {
      return rm(dir, { recursive: true, force: true });
    },
  };
}

// The armed clock step is observable from outside through the heartbeat, so a
// test can prove the preload is live before it writes the outbox file that
// depends on it, instead of sleeping and hoping.
async function waitForSteppedClock(session, aheadByMs) {
  return waitUntil(async () => {
    const info = JSON.parse(await readFile(path.join(session.dir, "server.json"), "utf8"));
    return info.heartbeat_at_ms > Date.now() + aheadByMs;
  }, "the server's wall clock to step forward", 2000);
}

test("I01 a forward wall-clock step must not disarm the 3 s interrupt throttle", { timeout: 5000 }, async () => {
  const preload = await withPreload();
  const session = await startServer(
    preload.env({ ZED_TEST_CLOCK_SHIFT_MS: "4000", ZED_CLAUDE_CHANNEL_HEARTBEAT_MS: "100" }),
  );
  const sigint = trapSigint();
  try {
    await handshake(session.child, session.rpc);
    await dropOutbox(session.dir, "int-1.json", { kind: "interrupt", reason: "first" });
    const first = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "interrupted"),
      "first interrupted inbox",
    );
    assert.ok(
      Math.abs(Date.now() - first.at_ms) < 3000,
      `harness broken: the first interrupt was already recorded with a stepped clock (actual at_ms ${first.at_ms}, test clock ${Date.now()})`,
    );

    // Move only the server's wall clock, 4 s forward — past the throttle
    // window — while roughly 300 ms of real time passes.
    await preload.arm();
    await waitForSteppedClock(session, 2000);
    await dropOutbox(session.dir, "int-2.json", { kind: "interrupt", reason: "second" });
    await waitUntil(
      async () => {
        const lines = await readInbox(session.dir);
        return (
          lines.some((line) => line.kind === "error" && line.outbox_file === "int-2.json") ||
          lines.filter((line) => line.kind === "interrupted").length > 1
        );
      },
      "a verdict for the second interrupt",
      1500,
    );

    const lines = await readInbox(session.dir);
    const interruptedLines = lines.filter((line) => line.kind === "interrupted");
    const rejected = lines.find(
      (line) => line.kind === "error" && line.outbox_file === "int-2.json",
    );
    assert.equal(
      interruptedLines.length,
      1,
      `a 4 s forward wall-clock step let a second SIGINT through ~300 ms after the first: actual ${
        interruptedLines.length
      } interrupted inbox lines (reasons ${JSON.stringify(
        interruptedLines.map((line) => line.reason),
      )}), expected 1 because only 300 ms of real time passed`,
    );
    assert.equal(
      rejected?.reason,
      "interrupt_throttled",
      `actual verdict for int-2.json ${JSON.stringify(
        rejected ?? null,
      )}, expected an error line with reason "interrupt_throttled"`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
    await preload.cleanup();
  }
});

// Boundary proof for I01's fix: the legal input a monotonic throttle could
// wrongly reject is a genuine second interrupt more than 3 s after the first.
test("I01 boundary a second interrupt more than 3 s later is still delivered", { timeout: 15000 }, async () => {
  const session = await startServer();
  const sigint = trapSigint();
  try {
    await handshake(session.child, session.rpc);
    await dropOutbox(session.dir, "int-1.json", { kind: "interrupt", reason: "first" });
    const first = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "interrupted"),
      "first interrupted inbox",
    );
    await delay(Math.max(0, 3150 - (Date.now() - first.at_ms)));
    await dropOutbox(session.dir, "int-2.json", { kind: "interrupt", reason: "second" });
    const both = await waitUntil(
      async () => {
        const lines = (await readInbox(session.dir)).filter((line) => line.kind === "interrupted");
        return lines.length === 2 ? lines : null;
      },
      "a second interrupted inbox line",
      1200,
    );
    assert.equal(
      both[1].reason,
      "second",
      `actual second interrupted line ${JSON.stringify(both[1])}, expected reason "second"`,
    );
    const errors = (await readInbox(session.dir)).filter((line) => line.kind === "error");
    assert.deepEqual(
      errors,
      [],
      `an interrupt 3.15 s after the previous one was rejected: actual ${JSON.stringify(
        errors,
      )}, expected no error lines because the 3 s throttle window had expired`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
  }
});

test("I02 an interrupt after reparenting must not signal the new parent", { timeout: 5000 }, async () => {
  const preload = await withPreload();
  const decoyMarker = path.join(preload.dir, "decoy-hit");
  const decoy = spawn(process.execPath, ["-e", DECOY_SOURCE], {
    env: { ...process.env, ZED_TEST_DECOY_MARKER: decoyMarker },
    stdio: ["ignore", "pipe", "ignore"],
  });
  let decoyReady = false;
  decoy.stdout.setEncoding("utf8");
  decoy.stdout.on("data", (chunk) => {
    if (chunk.includes("ready")) decoyReady = true;
  });
  await waitUntil(() => decoyReady, "the decoy process to install its SIGINT handler", 2000);
  const session = await startServer(
    preload.env({
      ZED_TEST_FAKE_PPID: String(decoy.pid),
      ZED_TEST_CLOCK_SHIFT_MS: "3000",
      ZED_CLAUDE_CHANNEL_HEARTBEAT_MS: "100",
    }),
  );
  const sigint = trapSigint();
  try {
    await handshake(session.child, session.rpc);
    const info = JSON.parse(await readFile(path.join(session.dir, "server.json"), "utf8"));
    assert.equal(
      info.claude_pid,
      process.pid,
      `actual claude_pid ${info.claude_pid}, expected the real parent ${process.pid}`,
    );

    // Arming swaps process.ppid to the decoy's pid and steps the clock by 3 s;
    // the heartbeat proves the swap is live before the outbox file is written.
    await preload.arm();
    await waitForSteppedClock(session, 1500);
    await dropOutbox(session.dir, "int-reparented.json", {
      kind: "interrupt",
      reason: "after reparent",
    });
    await waitUntil(
      async () => {
        const lines = await readInbox(session.dir);
        return lines.some(
          (line) =>
            (line.kind === "error" && line.outbox_file === "int-reparented.json") ||
            line.kind === "interrupted",
        );
      },
      "a verdict for the reparented interrupt",
      1500,
    );
    await delay(200);

    assert.equal(
      existsSync(decoyMarker),
      false,
      `the server sent SIGINT to a process that is not this Claude session: actual decoy pid ${decoy.pid} recorded a signal, expected no signal to any pid other than claude_pid ${process.pid}`,
    );
    assert.equal(
      decoy.exitCode,
      null,
      `an unrelated process was killed by the interrupt: actual decoy exitCode ${decoy.exitCode} signalCode ${decoy.signalCode}, expected it to still be running`,
    );
    const lines = await readInbox(session.dir);
    const verdict = lines.find(
      (line) => line.kind === "error" && line.outbox_file === "int-reparented.json",
    );
    assert.equal(
      verdict?.reason,
      "interrupt_unavailable",
      `actual verdict ${JSON.stringify(
        verdict ?? lines.find((line) => line.kind === "interrupted") ?? null,
      )}, expected an error line with reason "interrupt_unavailable"`,
    );
    assert.equal(
      sigint.count,
      0,
      `actual SIGINT count at the real parent ${sigint.count}, expected 0`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
    // The decoy exits on the signal this test is hunting, so `once(decoy,
    // "exit")` would wait for an event that already fired.
    if (decoy.exitCode === null && decoy.signalCode === null) {
      decoy.kill("SIGKILL");
      await once(decoy, "exit").catch(() => {});
    }
    await preload.cleanup();
  }
});

test("I03 a huge interrupt reason must not replace the whole inbox", { timeout: 5000 }, async () => {
  const session = await startServer();
  const sigint = trapSigint();
  try {
    await handshake(session.child, session.rpc);
    // Protocol traffic the panel still needs when the interrupt arrives.
    const preview = "P".repeat(64 * 1024);
    for (let index = 0; index < 4; index += 1) {
      send(session.child, {
        jsonrpc: "2.0",
        method: "notifications/claude/channel/permission_request",
        params: {
          request_id: `pad-${index}`,
          tool_name: "Bash",
          description: "d",
          input_preview: preview,
        },
      });
    }
    await waitUntil(
      async () =>
        (await readInbox(session.dir)).filter((line) => line.kind === "permission_request")
          .length === 4,
      "four padding permission_request lines",
    );

    // 4 000 032 bytes on disk: under OUTBOX_MAX_BYTES, so the file itself is legal.
    const reason = "R".repeat(4_000_000);
    await dropOutbox(session.dir, "int-huge.json", { kind: "interrupt", reason });
    const interrupted = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "interrupted"),
      "interrupted inbox",
      2000,
    );

    const lines = await readInbox(session.dir);
    assert.equal(
      lines.some((line) => line.kind === "ready"),
      true,
      `one interrupt outbox file replaced the whole inbox: actual kinds ${JSON.stringify(
        lines.map((line) => line.kind),
      )}, expected the ready line to survive`,
    );
    assert.equal(
      lines.filter((line) => line.kind === "permission_request").length,
      4,
      `open permission requests were erased from inbox.jsonl by one interrupt: actual ${
        lines.filter((line) => line.kind === "permission_request").length
      } permission_request lines, expected 4`,
    );
    assert.ok(
      interrupted.reason.length <= 512,
      `actual interrupted.reason length ${interrupted.reason.length}, expected at most 512 characters`,
    );
    assert.equal(
      sigint.count,
      1,
      `actual SIGINT count ${sigint.count}, expected 1: an oversized reason must not stop the interrupt itself`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
  }
});

// Boundary proof for I03's fix: the legal input the 512-character cap could
// wrongly truncate is a reason exactly at the limit.
test("I03 boundary an interrupt reason at the 512-character limit is copied verbatim", { timeout: 5000 }, async () => {
  const session = await startServer();
  const sigint = trapSigint();
  try {
    await handshake(session.child, session.rpc);
    const reason = "Z".repeat(512);
    await dropOutbox(session.dir, "int-512.json", { kind: "interrupt", reason });
    const interrupted = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "interrupted"),
      "interrupted inbox",
    );
    assert.equal(
      interrupted.reason,
      reason,
      `a 512-character reason was altered: actual length ${interrupted.reason.length}, expected the 512-character reason unchanged`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
  }
});

// Legal input the 512-character cap could wrongly cut: a BMP prefix of 511
// characters plus an emoji (one Unicode code point, two UTF-16 code units).
// String#slice(0, 512) keeps the high surrogate; JSON.stringify then emits
// \ud83d, which Node accepts and serde_json (Zed's inbox reader) rejects.
test("V01 an interrupt reason must not be sliced in the middle of a surrogate pair", { timeout: 5000 }, async () => {
  const session = await startServer();
  const sigint = trapSigint();
  try {
    await handshake(session.child, session.rpc);
    const reason = `${"A".repeat(511)}😀TAIL`;
    await dropOutbox(session.dir, "int-emoji.json", { kind: "interrupt", reason });
    await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "interrupted"),
      "interrupted inbox",
    );
    const raw = await readFile(path.join(session.dir, "inbox.jsonl"), "utf8");
    const interruptedLine = raw.split("\n").find((line) => line.includes('"kind":"interrupted"'));
    assert.equal(
      /\\ud83d/i.test(interruptedLine ?? ""),
      false,
      `inbox.jsonl carried a lone UTF-16 high-surrogate escape that serde_json rejects: actual line ${interruptedLine}`,
    );
    const interrupted = JSON.parse(interruptedLine);
    const expected = `${"A".repeat(511)}😀`;
    assert.equal(
      interrupted.reason,
      expected,
      `actual reason ${JSON.stringify(interrupted.reason)} (utf16 length ${
        interrupted.reason.length
      }, last charCode ${interrupted.reason.charCodeAt(interrupted.reason.length - 1)?.toString(16)}), expected the 511-character prefix plus the complete emoji (512 code points, no lone surrogate)`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
  }
});

test("I04 a parked outbox name is no longer blacklisted once the stuck entry is gone", { timeout: 5000 }, async (t) => {
  if (typeof process.getuid === "function" && process.getuid() === 0) {
    t.skip("running as root: a read-only directory would not deny rename");
    return;
  }
  const session = await startServer();
  const outbox = path.join(session.dir, "outbox");
  try {
    await handshake(session.child, session.rpc);
    // A self-referential symlink is listed by readdir but makes statSync throw
    // ELOOP; with the directory read-only the .bad rename fails too, so the
    // name is parked as "unreadable".
    symlinkSync("0001.json", path.join(outbox, "0001.json"));
    chmodSync(outbox, 0o500);
    await waitUntil(
      async () => inboxErrors(await readInbox(session.dir), "0001.json").length === 1,
      "the stuck entry to be reported once",
    );
    await delay(400);
    const whileStuck = inboxErrors(await readInbox(session.dir), "0001.json");
    assert.equal(
      whileStuck.length,
      1,
      `the stuck entry was retried on every poll: actual ${whileStuck.length} error lines, expected 1`,
    );

    chmodSync(outbox, 0o700);
    unlinkSync(path.join(outbox, "0001.json"));
    await delay(500);
    symlinkSync("0001.json", path.join(outbox, "0001.json"));
    await delay(700);

    const names = readdirSync(outbox).sort();
    const errors = inboxErrors(await readInbox(session.dir), "0001.json");
    assert.equal(
      errors.length,
      2,
      `a parked outbox name stayed blacklisted after its file was gone: actual ${errors.length} error lines for 0001.json, expected 2 (one per stuck entry, because the parked name must be pruned once readdir no longer lists it)`,
    );
    assert.deepEqual(
      names,
      ["0001.json.bad"],
      `the entry at a previously parked name was silently ignored instead of quarantined: actual outbox contents ${JSON.stringify(
        names,
      )}, expected ["0001.json.bad"]`,
    );
  } finally {
    chmodSync(outbox, 0o700);
    await stopServer(session);
  }
});

// Boundary proof for the 10 s staleness guard: an interrupt queued before the
// handshake and flushed 8.5 s later is still inside the window and must fire.
test("stale boundary an interrupt whose mtime is 8.5 s old is still delivered", { timeout: 5000 }, async () => {
  const session = await startServer();
  const sigint = trapSigint();
  try {
    await initialize(session.child, session.rpc);
    await dropOutbox(session.dir, "int-aged.json", {
      kind: "interrupt",
      reason: "queued before the handshake",
    });
    const aged = new Date(Date.now() - 8500);
    utimesSync(path.join(session.dir, "outbox", "int-aged.json"), aged, aged);
    notifyInitialized(session.child);
    const interrupted = await waitUntil(
      async () => (await readInbox(session.dir)).find((line) => line.kind === "interrupted"),
      "interrupted inbox",
      1500,
    );
    assert.equal(
      interrupted.reason,
      "queued before the handshake",
      `actual interrupted line ${JSON.stringify(interrupted)}, expected reason "queued before the handshake"`,
    );
    const errors = (await readInbox(session.dir)).filter((line) => line.kind === "error");
    assert.deepEqual(
      errors,
      [],
      `an interrupt 1.5 s inside the 10 s staleness window was rejected: actual ${JSON.stringify(
        errors,
      )}, expected no error lines`,
    );
    assert.equal(
      sigint.count,
      1,
      `actual SIGINT count ${sigint.count}, expected 1`,
    );
  } finally {
    sigint.restore();
    await stopServer(session);
  }
});
