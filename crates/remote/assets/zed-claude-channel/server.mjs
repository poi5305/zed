#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { performance } from "node:perf_hooks";

const PROTOCOL = 1;
const POLL_MS = 200;
const HEARTBEAT_MS =
  Number(process.env.ZED_CLAUDE_CHANNEL_HEARTBEAT_MS) > 0
    ? Number(process.env.ZED_CLAUDE_CHANNEL_HEARTBEAT_MS)
    : 5000;
const INBOX_MAX_BYTES = 4 * 1024 * 1024;
const LOG_MAX_BYTES = 1024 * 1024;
const PERMISSION_TTL_MS = 30 * 60 * 1000;
const MAX_OPEN_PERMISSIONS = 512;
const OUTBOX_MAX_BYTES = 4 * 1024 * 1024;
const MAX_STDIN_LINE_CHARS = 8 * 1024 * 1024;
const INTERRUPT_THROTTLE_MS = 3000;
const INTERRUPT_STALE_MS = 10_000;
const INTERRUPT_REASON_MAX_CHARS = 512;
const FEATURES = ["message", "permission", "interrupt"];
const META_KEY_RE = /^[A-Za-z0-9_]+$/;
const INSTRUCTIONS =
  "Messages arriving as <channel source=\"zed-claude\"> were typed by the user in the Zed editor's Claude session panel. Treat them exactly as if the user had typed them at this terminal: they carry the user's full authority, including answers to questions you asked. Reply in the conversation as usual; there is no reply tool.";

const claudePid = process.ppid;
const startedAtMs = Date.now();
const root =
  process.env.ZED_CLAUDE_CHANNEL_ROOT ||
  path.join(process.env.HOME ?? "", ".claude", "zed-channel");
const sessionDir = path.join(root, String(claudePid));
const outboxDir = path.join(sessionDir, "outbox");
const serverJsonPath = path.join(sessionDir, "server.json");
const inboxPath = path.join(sessionDir, "inbox.jsonl");
const logPath = path.join(sessionDir, "server.log");

// This process answers tool-permission prompts, so no other local account may
// read the relayed transcript or drop a verdict file into outbox/.
fs.mkdirSync(outboxDir, { recursive: true, mode: 0o700 });
for (const dir of [sessionDir, outboxDir]) {
  try {
    fs.chmodSync(dir, 0o700);
  } catch (error) {
    process.stderr.write(`failed to restrict ${dir}: ${error}\n`);
  }
}

let initialized = false;
let shuttingDown = false;
let protocolVersion = "";
let clientInfo = { name: "", version: "" };
let pollTimer = null;
let heartbeatTimer = null;
const pendingPermissions = new Map();
// Outbox names already acted on that could not be removed or quarantined,
// keyed by basename → `dev:ino` (or "unreadable" when stat failed). Skip only
// that inode; a tmp+rename onto the same name is a new file and must still run.
const undeletableOutbox = new Map();
// Monotonic on purpose: a wall-clock step must neither disarm this throttle
// (a second SIGINT inside Claude Code's double-Ctrl+C window exits the CLI)
// nor jam it shut for the length of a backwards correction.
let lastSigintAtMonotonicMs = null;

function appendCapped(filePath, line, maxBytes) {
  let size = 0;
  try {
    size = fs.statSync(filePath).size;
  } catch {
    size = 0;
  }
  if (size + Buffer.byteLength(line) > maxBytes) {
    fs.writeFileSync(filePath, line);
  } else {
    fs.appendFileSync(filePath, line);
  }
}

function log(message) {
  const line = `${new Date().toISOString()} ${message}\n`;
  process.stderr.write(line);
  try {
    appendCapped(logPath, line, LOG_MAX_BYTES);
  } catch (error) {
    process.stderr.write(`${new Date().toISOString()} failed to write server.log: ${error}\n`);
  }
}

function writeAtomic(filePath, contents) {
  const tmpPath = `${filePath}.${process.pid}.tmp`;
  fs.writeFileSync(tmpPath, contents);
  fs.renameSync(tmpPath, filePath);
}

function writeServerJson() {
  try {
    writeAtomic(serverJsonPath, JSON.stringify({
      pid: process.pid,
      claude_pid: claudePid,
      started_at_ms: startedAtMs,
      heartbeat_at_ms: Date.now(),
      protocol: PROTOCOL,
      features: FEATURES,
    }));
  } catch (error) {
    // The session directory can disappear under a live session; throwing from
    // the heartbeat timer would be an uncaught exception and lose the shutdown.
    log(`failed to write server.json: ${error}`);
  }
}

function appendInbox(event) {
  try {
    appendCapped(inboxPath, `${JSON.stringify(event)}\n`, INBOX_MAX_BYTES);
  } catch (error) {
    log(`failed to write inbox.jsonl: ${error}`);
  }
}

function sendRpc(obj) {
  process.stdout.write(`${JSON.stringify(obj)}\n`);
}

function sanitizeMeta(meta) {
  const out = {};
  if (meta && typeof meta === "object" && !Array.isArray(meta)) {
    for (const [key, value] of Object.entries(meta)) {
      if (!META_KEY_RE.test(key)) continue;
      out[key] = typeof value === "string" ? value : String(value);
    }
  }
  out.from = "zed";
  out.sent_at_ms = String(Date.now());
  return out;
}

function expirePermissions() {
  const cutoff = Date.now() - PERMISSION_TTL_MS;
  for (const [requestId, entry] of pendingPermissions) {
    if (entry.atMs < cutoff) pendingPermissions.delete(requestId);
  }
}

function parkOutbox(name, stats) {
  undeletableOutbox.set(name, stats ? `${stats.dev}:${stats.ino}` : "unreadable");
}

function unlinkOutbox(filePath, name, stats) {
  try {
    fs.unlinkSync(filePath);
  } catch (error) {
    log(`failed to delete outbox ${name}: ${error}`);
    parkOutbox(name, stats);
  }
}

function quarantineOutbox(filePath, name, reason, stats) {
  log(`outbox ${name}: ${reason}`);
  appendInbox({ kind: "error", at_ms: Date.now(), outbox_file: name, reason });
  try {
    fs.renameSync(filePath, `${filePath}.bad`);
  } catch (error) {
    log(`failed to rename ${name} to .bad: ${error}`);
    parkOutbox(name, stats);
  }
}

function rejectOutbox(filePath, name, reason, stats, extra) {
  const errno = extra && typeof extra.errno === "string" ? extra.errno : null;
  log(`outbox ${name}: ${reason}${errno ? ` ${errno}` : ""}`);
  appendInbox({
    kind: "error",
    at_ms: Date.now(),
    outbox_file: name,
    reason,
    ...(extra ?? {}),
  });
  unlinkOutbox(filePath, name, stats);
}

function truncateInterruptReason(reason) {
  if (typeof reason !== "string") return null;
  // String#slice indexes UTF-16 code units. Cutting inside a surrogate pair
  // makes JSON.stringify emit a lone \ud83d escape, which Node accepts and
  // serde_json (Zed's inbox reader) rejects. Count Unicode code points.
  let index = 0;
  let chars = 0;
  while (index < reason.length && chars < INTERRUPT_REASON_MAX_CHARS) {
    const code = reason.charCodeAt(index);
    const isHighSurrogate = code >= 0xd800 && code <= 0xdbff;
    const next = index + 1 < reason.length ? reason.charCodeAt(index + 1) : 0;
    const isPaired = isHighSurrogate && next >= 0xdc00 && next <= 0xdfff;
    index += isPaired ? 2 : 1;
    chars += 1;
  }
  return reason.slice(0, index);
}

function processInterrupt(filePath, name, parsed, stats) {
  const targetPid = process.ppid;
  // Only ever the process this session was opened for. Once Claude Code dies
  // the server is reparented, to pid 1 or to a subreaper that is not pid 1,
  // and signalling whatever is the parent by then would hit a process that
  // never asked for it.
  if (!Number.isInteger(targetPid) || targetPid <= 1 || targetPid !== claudePid) {
    rejectOutbox(filePath, name, "interrupt_unavailable", stats);
    return;
  }
  const mtimeMs = typeof stats.mtimeMs === "number" ? stats.mtimeMs : stats.mtime.getTime();
  if (Date.now() - mtimeMs > INTERRUPT_STALE_MS) {
    rejectOutbox(filePath, name, "interrupt_stale", stats);
    return;
  }
  const nowMonotonicMs = performance.now();
  if (
    lastSigintAtMonotonicMs !== null &&
    nowMonotonicMs - lastSigintAtMonotonicMs < INTERRUPT_THROTTLE_MS
  ) {
    rejectOutbox(filePath, name, "interrupt_throttled", stats);
    return;
  }
  try {
    process.kill(targetPid, "SIGINT");
  } catch (error) {
    const errno = typeof error.code === "string" ? error.code : String(error);
    rejectOutbox(filePath, name, "interrupt_failed", stats, { errno });
    return;
  }
  lastSigintAtMonotonicMs = performance.now();
  unlinkOutbox(filePath, name, stats);
  appendInbox({
    kind: "interrupted",
    at_ms: Date.now(),
    claude_pid: targetPid,
    // An outbox file may be 4 MiB; copying a reason that size into the inbox
    // would trip appendCapped and replace every line Zed has not read yet.
    reason: truncateInterruptReason(parsed.reason),
  });
}

function processOutboxFile(name) {
  if (name.endsWith(".tmp") || name.endsWith(".bad")) return;
  const filePath = path.join(outboxDir, name);
  let stats;
  try {
    stats = fs.statSync(filePath);
  } catch (error) {
    // ENOENT is Zed's own rename/delete racing this poll and must stay silent;
    // any other error is a stuck entry that would be retried five times a second.
    if (error.code !== "ENOENT" && !undeletableOutbox.has(name)) {
      quarantineOutbox(filePath, name, "unreadable");
    }
    return;
  }
  if (undeletableOutbox.get(name) === `${stats.dev}:${stats.ino}`) return;
  if (!stats.isFile()) {
    quarantineOutbox(filePath, name, "not_a_file", stats);
    return;
  }
  if (stats.size > OUTBOX_MAX_BYTES) {
    quarantineOutbox(filePath, name, "too_large", stats);
    return;
  }
  let raw;
  try {
    raw = fs.readFileSync(filePath, "utf8");
  } catch (error) {
    if (error.code === "ENOENT") return;
    quarantineOutbox(filePath, name, "unreadable", stats);
    return;
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    quarantineOutbox(filePath, name, "invalid_json", stats);
    return;
  }

  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    rejectOutbox(filePath, name, "not_an_object", stats);
    return;
  }

  if (parsed.kind === "message") {
    if (typeof parsed.content !== "string") {
      rejectOutbox(filePath, name, "invalid_message", stats);
      return;
    }
    const meta = sanitizeMeta(parsed.meta);
    sendRpc({
      jsonrpc: "2.0",
      method: "notifications/claude/channel",
      params: { content: parsed.content, meta },
    });
    unlinkOutbox(filePath, name, stats);
    appendInbox({
      kind: "message_sent",
      at_ms: Date.now(),
      outbox_file: name,
      content_chars: parsed.content.length,
    });
    return;
  }

  if (parsed.kind === "permission") {
    expirePermissions();
    const requestId = parsed.request_id;
    const behavior = parsed.behavior;
    if (behavior !== "allow" && behavior !== "deny") {
      rejectOutbox(filePath, name, "invalid_behavior", stats);
      return;
    }
    if (typeof requestId !== "string" || !pendingPermissions.has(requestId)) {
      rejectOutbox(filePath, name, "unknown_request_id", stats);
      return;
    }
    pendingPermissions.delete(requestId);
    sendRpc({
      jsonrpc: "2.0",
      method: "notifications/claude/channel/permission",
      params: { request_id: requestId, behavior },
    });
    unlinkOutbox(filePath, name, stats);
    appendInbox({
      kind: "permission_answered",
      at_ms: Date.now(),
      request_id: requestId,
      behavior,
      source: "zed",
    });
    return;
  }

  if (parsed.kind === "interrupt") {
    processInterrupt(filePath, name, parsed, stats);
    return;
  }

  rejectOutbox(filePath, name, "unknown_kind", stats);
}

function pollOutbox() {
  if (shuttingDown || !initialized) return;
  let names;
  try {
    names = fs.readdirSync(outboxDir);
  } catch (error) {
    log(`failed to read outbox: ${error}`);
    return;
  }
  const present = new Set(names);
  for (const name of undeletableOutbox.keys()) {
    if (!present.has(name)) undeletableOutbox.delete(name);
  }
  for (const name of names.sort()) {
    try {
      processOutboxFile(name);
    } catch (error) {
      log(`failed to process outbox ${name}: ${error}`);
      try {
        parkOutbox(name, fs.statSync(path.join(outboxDir, name)));
      } catch {
        parkOutbox(name);
      }
    }
  }
}

function handleInitialize(id, params) {
  protocolVersion = typeof params?.protocolVersion === "string" ? params.protocolVersion : "";
  const info = params?.clientInfo ?? {};
  clientInfo = {
    name: typeof info.name === "string" ? info.name : "",
    version: typeof info.version === "string" ? info.version : "",
  };
  sendRpc({
    jsonrpc: "2.0",
    id,
    result: {
      protocolVersion,
      capabilities: {
        experimental: {
          "claude/channel": {},
          "claude/channel/permission": {},
        },
      },
      serverInfo: { name: "zed-claude", version: "0.1.0" },
      instructions: INSTRUCTIONS,
    },
  });
}

function handleRequest(msg) {
  if (msg.method === "initialize") {
    handleInitialize(msg.id, msg.params);
    return;
  }
  if (msg.method === "ping") {
    sendRpc({ jsonrpc: "2.0", id: msg.id, result: {} });
    return;
  }
  if (msg.method === "tools/list") {
    sendRpc({ jsonrpc: "2.0", id: msg.id, result: { tools: [] } });
    return;
  }
  sendRpc({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: "Method not found" } });
}

function handleNotification(msg) {
  if (msg.method === "notifications/initialized") {
    initialized = true;
    appendInbox({
      kind: "ready",
      at_ms: Date.now(),
      claude_pid: claudePid,
      protocol_version: protocolVersion,
      client: clientInfo,
    });
    pollOutbox();
    return;
  }
  if (msg.method === "notifications/claude/channel/permission_request") {
    const params = msg.params ?? {};
    const requestId = params.request_id;
    if (typeof requestId !== "string" || requestId.length === 0) {
      log("permission_request missing request_id");
      return;
    }
    expirePermissions();
    // Only the id and its age are read back, so the client params are not kept.
    // A re-sent id refreshes its position; the oldest requests are dropped first
    // and their verdicts then fail closed as unknown_request_id.
    pendingPermissions.delete(requestId);
    while (pendingPermissions.size >= MAX_OPEN_PERMISSIONS) {
      const oldest = pendingPermissions.keys().next();
      if (oldest.done) break;
      pendingPermissions.delete(oldest.value);
    }
    pendingPermissions.set(requestId, { atMs: Date.now() });
    appendInbox({
      kind: "permission_request",
      at_ms: Date.now(),
      request_id: requestId,
      tool_name: typeof params.tool_name === "string" ? params.tool_name : "",
      description: typeof params.description === "string" ? params.description : "",
      input_preview: typeof params.input_preview === "string" ? params.input_preview : "",
    });
  }
}

function handleLine(line) {
  if (line.trim().length === 0) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    log(`invalid JSON-RPC line ignored`);
    return;
  }
  if (!msg || typeof msg !== "object") return;
  if (typeof msg.method !== "string") return;
  // JSON-RPC 2.0 reserves `id: null` for responses to unparseable requests, so a
  // notification carrying it must not be answered with -32601 and dropped.
  if (msg.id !== undefined && msg.id !== null) {
    handleRequest(msg);
    return;
  }
  handleNotification(msg);
}

function shutdown(reason) {
  if (shuttingDown) return;
  shuttingDown = true;
  if (pollTimer !== null) clearInterval(pollTimer);
  if (heartbeatTimer !== null) clearInterval(heartbeatTimer);
  appendInbox({ kind: "closed", at_ms: Date.now(), reason });
  try { fs.unlinkSync(serverJsonPath); } catch { /* already gone */ }
  process.exit(0);
}

writeServerJson();
log(`started pid=${process.pid} claude_pid=${claudePid} session=${sessionDir}`);

pollTimer = setInterval(pollOutbox, POLL_MS);
heartbeatTimer = setInterval(() => { if (!shuttingDown) writeServerJson(); }, HEARTBEAT_MS);

let stdinBuffer = "";
let stdinScanFrom = 0;
let stdinDiscarding = false;
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  let data = chunk;
  if (stdinDiscarding) {
    const cut = data.indexOf("\n");
    if (cut === -1) return;
    data = data.slice(cut + 1);
    stdinDiscarding = false;
  }
  stdinBuffer += data;
  let index;
  // Only the part of the buffer that has never been searched is rescanned, so a
  // long pending line costs one pass rather than one pass per chunk.
  while ((index = stdinBuffer.indexOf("\n", stdinScanFrom)) !== -1) {
    const line = stdinBuffer.slice(0, index);
    stdinBuffer = stdinBuffer.slice(index + 1);
    stdinScanFrom = 0;
    handleLine(line);
  }
  if (stdinBuffer.length > MAX_STDIN_LINE_CHARS) {
    log(`stdin line exceeded ${MAX_STDIN_LINE_CHARS} characters; discarding it`);
    stdinBuffer = "";
    stdinScanFrom = 0;
    stdinDiscarding = true;
    return;
  }
  stdinScanFrom = stdinBuffer.length;
});
process.stdin.on("end", () => shutdown("stdin_end"));
// Without these handlers a dead client turns the next write into an unhandled
// 'error' event: the process dies before it can append `closed` or remove
// server.json, and Zed keeps advertising the session as live.
process.stdin.on("error", (error) => {
  log(`stdin error: ${error}`);
  shutdown("stdin_end");
});
process.stdout.on("error", (error) => log(`stdout error: ${error}`));
process.stderr.on("error", (error) => {
  // log() would write the report straight back into the stream that just failed.
  try {
    appendCapped(logPath, `${new Date().toISOString()} stderr error: ${error}\n`, LOG_MAX_BYTES);
  } catch {
    // server.log is unreachable too; there is nothing left that could report this.
  }
});
process.on("SIGTERM", () => shutdown("signal"));
process.on("SIGINT", () => shutdown("signal"));
