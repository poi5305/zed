#!/usr/bin/env node
// End-to-end check of the Zed Web ssh relay without a browser and without an sshd.
//
// It starts the real zed-web-server with --allow-ssh and a PATH that puts web/test-support/fake-ssh
// first (as `ssh`, `scp` and `sftp`), so "connecting to localhost" means running everything on
// this machine. Then it speaks the same wire protocol the wasm client does:
//
//   POST /login -> /rpc RemoteSsh::connect -> RemoteSsh::open_channel -> /remote/channel
//
// and proves that protobuf envelopes flow through the relay to a real `remote_server proxy`, and
// that the proxy's exit status comes back as close code 1000 with reason {"exit_code":N}
// (spec docs/web-zed-remote-spec.md 4.4 and 7.2).
//
//   node web/test-support/remote-e2e.mjs                  real remote_server, all scenarios
//   node web/test-support/remote-e2e.mjs --stub-remote    a shell stub instead of remote_server
//   node web/test-support/remote-e2e.mjs --stub-remote --proxy-exit 37
//
// Options:
//   --server <path>          zed-web-server binary (default target/debug/zed-web-server)
//   --remote-server <path>   remote_server binary (default: first of target/release/remote_server,
//                            target/web-native/remote-server-stripped/<triple>/remote_server)
//   --stub-remote            bundle a shell script that answers `version` and exits from `proxy`;
//                            for when no usable remote_server is built. The envelope scenario is
//                            skipped, the exit-code scenario checks the stub's status.
//   --proxy-exit <n>         the status the stub's proxy exits with (default 0; needs --stub-remote)
//   --skip-password          skip the askpass/password scenarios
//   --keep                   keep the sandbox directory (always kept on failure)
//
// The sandbox lives under target/remote-e2e so the bundle can hard-link the two large binaries.
// A hard link, not a symlink: the server finds its bundle next to std::env::current_exe(), and a
// symlink would resolve to the real directory. The fake remote home is the exception: it lives in
// a short directory under /tmp, because remote_server binds unix sockets under
// $HOME/.local/share/zed/server_state/<identifier>/ and sun_path holds only 108 bytes (104 on
// macOS). Under target/ those paths are ~140 bytes, bind fails, and the proxy reports only
// "failed to spawn server" ten seconds later. A real ssh login has HOME=/home/<user>.
//
// No dependencies, deliberately; the protobuf needed is a few varints.

import { spawn, execFileSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(import.meta.dirname, "../..");
const fakeSsh = path.join(import.meta.dirname, "fake-ssh");

const PROTO_FIELD = { ack: 5, error: 6, ping: 7, remoteStarted: 381 };
const PROTO_NAME = new Map([
  [PROTO_FIELD.ack, "Ack"],
  [PROTO_FIELD.error, "Error"],
  [PROTO_FIELD.ping, "Ping"],
  [PROTO_FIELD.remoteStarted, "RemoteStarted"],
]);

// ---------------------------------------------------------------- options

function parseOptions(argv) {
  const options = {
    server: path.join(repositoryRoot, "target/debug/zed-web-server"),
    remoteServer: process.env.REMOTE_SERVER_BINARY ?? null,
    stubRemote: false,
    proxyExit: 0,
    skipPassword: false,
    keep: false,
  };
  for (let index = 0; index < argv.length; index++) {
    const argument = argv[index];
    if (argument === "--server") options.server = path.resolve(argv[++index]);
    else if (argument === "--remote-server") options.remoteServer = path.resolve(argv[++index]);
    else if (argument === "--stub-remote") options.stubRemote = true;
    else if (argument === "--proxy-exit") options.proxyExit = Number(argv[++index]);
    else if (argument === "--skip-password") options.skipPassword = true;
    else if (argument === "--keep") options.keep = true;
    else throw new Error(`unknown argument ${argument}`);
  }
  if (!Number.isInteger(options.proxyExit) || options.proxyExit < 0 || options.proxyExit > 255) {
    throw new Error("--proxy-exit must be an integer from 0 to 255");
  }
  if (options.proxyExit !== 0 && !options.stubRemote) {
    throw new Error("--proxy-exit needs --stub-remote: the real remote_server decides its own exit status");
  }
  return options;
}

function hostPlatform() {
  const operatingSystem = { linux: "linux", darwin: "macos" }[process.platform];
  const architecture = { x64: "x86_64", arm64: "aarch64" }[process.arch];
  if (!operatingSystem || !architecture) {
    throw new Error(`no remote_server platform for node ${process.platform}/${process.arch}`);
  }
  return { os: operatingSystem, arch: architecture };
}

// ---------------------------------------------------------------- sandbox

const cleanupActions = [];
let sandboxKept = false;

function runCleanup() {
  while (cleanupActions.length) {
    const action = cleanupActions.pop();
    try {
      action();
    } catch (error) {
      console.error(`cleanup step failed: ${error.message}`);
    }
  }
}

process.on("exit", runCleanup);
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    runCleanup();
    process.exit(130);
  });
}

function linkOrCopy(source, destination) {
  try {
    fs.linkSync(source, destination);
  } catch (error) {
    if (error.code !== "EXDEV" && error.code !== "EPERM") throw error;
    fs.copyFileSync(source, destination);
  }
}

function sha256Of(file) {
  const hash = crypto.createHash("sha256");
  const descriptor = fs.openSync(file, "r");
  try {
    const buffer = Buffer.alloc(1 << 20);
    for (;;) {
      const read = fs.readSync(descriptor, buffer, 0, buffer.length, null);
      if (read === 0) break;
      hash.update(buffer.subarray(0, read));
    }
  } finally {
    fs.closeSync(descriptor);
  }
  return hash.digest("hex");
}

function findRealRemoteServer(platform) {
  const candidates = [path.join(repositoryRoot, "target/release/remote_server")];
  const strippedRoot = path.join(repositoryRoot, "target/web-native/remote-server-stripped");
  const marker = platform.os === "linux" ? "linux" : "apple-darwin";
  if (fs.existsSync(strippedRoot)) {
    for (const triple of fs.readdirSync(strippedRoot)) {
      if (triple.startsWith(`${platform.arch}-`) && triple.includes(marker)) {
        candidates.push(path.join(strippedRoot, triple, "remote_server"));
      }
    }
  }
  return candidates.find((candidate) => fs.existsSync(candidate)) ?? null;
}

function lastNonEmptyLine(text) {
  const lines = text.split("\n").map((line) => line.trim()).filter(Boolean);
  return lines.at(-1) ?? "";
}

// Mirrors ServerPaths::new in crates/remote_server/src/server.rs with the longest identifier
// RemoteSsh::open_channel accepts (40 bytes). "Zed Nightly" is the longest macOS APP_NAME.
const LONGEST_IDENTIFIER = "x".repeat(40);
function longestRemoteSocketPath(remoteHome) {
  const stateDirectory =
    process.platform === "darwin"
      ? "Library/Application Support/Zed Nightly/server_state"
      : ".local/share/zed/server_state";
  return path.join(remoteHome, stateDirectory, LONGEST_IDENTIFIER, "stdout.sock");
}

function createRemoteHome() {
  // sun_path size minus the terminating NUL.
  const limit = process.platform === "darwin" ? 103 : 107;
  const tried = [];
  for (const parent of ["/tmp", os.tmpdir()]) {
    let candidate;
    try {
      candidate = fs.mkdtempSync(path.join(parent, "zed-e2e-"));
    } catch (error) {
      tried.push(`${parent}: ${error.message}`);
      continue;
    }
    const longest = longestRemoteSocketPath(candidate);
    if (Buffer.byteLength(longest) <= limit) return candidate;
    fs.rmSync(candidate, { recursive: true, force: true });
    tried.push(`${longest} is ${Buffer.byteLength(longest)} bytes`);
  }
  throw new Error(
    `no directory gives the fake remote home unix socket paths of at most ${limit} bytes ` +
      `(remote_server's bind would fail with "path must be shorter than SUN_LEN"): ${tried.join("; ")}`,
  );
}

function createSandbox(options) {
  if (!fs.existsSync(options.server)) {
    throw new Error(`${options.server} does not exist; run: cargo build -p zed_web_server`);
  }
  const platform = hostPlatform();
  const parent = path.join(repositoryRoot, "target/remote-e2e");
  fs.mkdirSync(parent, { recursive: true });
  const directory = fs.mkdtempSync(path.join(parent, "run-"));
  const remoteHome = createRemoteHome();
  const sandbox = {
    directory,
    platform,
    binDirectory: path.join(directory, "bin"),
    bundleDirectory: path.join(directory, "bin/remote"),
    fakeBinDirectory: path.join(directory, "fake-bin"),
    remoteHome,
    serverHome: path.join(directory, "server-home"),
    root: path.join(directory, "root"),
    staticRoot: path.join(directory, "static"),
    fakeSshLog: path.join(directory, "fake-ssh.log"),
    serverLog: path.join(directory, "server.log"),
    manifestCommit: null,
    remoteServerDescription: null,
  };
  cleanupActions.push(() => {
    killRemoteProcesses(sandbox);
    if (!options.keep && !sandboxKept) {
      fs.rmSync(directory, { recursive: true, force: true });
      fs.rmSync(remoteHome, { recursive: true, force: true });
    }
  });
  for (const name of ["binDirectory", "bundleDirectory", "fakeBinDirectory", "remoteHome", "serverHome", "root", "staticRoot"]) {
    fs.mkdirSync(sandbox[name], { recursive: true });
  }
  fs.writeFileSync(sandbox.fakeSshLog, "");
  for (const name of ["ssh", "scp", "sftp"]) {
    fs.symlinkSync(fakeSsh, path.join(sandbox.fakeBinDirectory, name));
  }
  linkOrCopy(options.server, path.join(sandbox.binDirectory, "zed-web-server"));

  const bundledFile = `remote_server-${platform.os}-${platform.arch}`;
  const bundledPath = path.join(sandbox.bundleDirectory, bundledFile);
  if (options.stubRemote) {
    sandbox.manifestCommit = "e2e-stub-0000000000000000000000000000000000";
    fs.writeFileSync(
      bundledPath,
      `#!/bin/sh\ncase "$1" in\n  version) echo ${sandbox.manifestCommit} ;;\n  proxy) exit ${options.proxyExit} ;;\n  *) exit 64 ;;\nesac\n`,
      { mode: 0o755 },
    );
    sandbox.remoteServerDescription = `shell stub (proxy exits ${options.proxyExit})`;
  } else {
    const source = options.remoteServer ?? findRealRemoteServer(platform);
    if (!source || !fs.existsSync(source)) {
      throw new Error(
        "no remote_server binary found (looked in target/release and target/web-native/remote-server-stripped); " +
          "pass --remote-server <path> or use --stub-remote",
      );
    }
    let versionOutput;
    try {
      versionOutput = execFileSync(source, ["version"], { encoding: "utf8", timeout: 60000 });
    } catch (error) {
      throw new Error(`${source} version failed (${error.message}); pass --remote-server or use --stub-remote`);
    }
    sandbox.manifestCommit = lastNonEmptyLine(versionOutput);
    linkOrCopy(source, bundledPath);
    fs.chmodSync(bundledPath, 0o755);
    sandbox.remoteServerDescription = `${source} (version ${sandbox.manifestCommit})`;
  }
  const manifest = {
    commit: sandbox.manifestCommit,
    binaries: [{ os: platform.os, arch: platform.arch, file: bundledFile, sha256: sha256Of(bundledPath) }],
  };
  fs.writeFileSync(path.join(sandbox.bundleDirectory, "manifest.json"), JSON.stringify(manifest, null, 2));
  return sandbox;
}

function killPid(pid) {
  if (!Number.isInteger(pid) || pid <= 1 || pid === process.pid) return;
  try {
    process.kill(pid, "SIGKILL");
  } catch (error) {
    if (error.code !== "ESRCH") throw error;
  }
}

// Everything the fake host starts outlives the zed-web-server that asked for it: the proxy
// daemonizes remote_server, kill_on_drop SIGKILLs only the `sh` that fake-ssh became (the proxy
// is its child), and a fake master is a bare `sleep`. All of them inherit FAKE_SSH_HOME, which
// names this run's remote home and nothing else, so on Linux that is what identifies them.
function killRemoteProcesses(sandbox) {
  const marker = `FAKE_SSH_HOME=${sandbox.remoteHome}\0`;
  let processIds = [];
  try {
    processIds = fs.readdirSync("/proc").filter((name) => /^\d+$/.test(name));
  } catch {
    // No /proc (macOS): fall back to what the files name.
  }
  for (const processId of processIds) {
    let environment;
    try {
      environment = fs.readFileSync(`/proc/${processId}/environ`, "latin1");
    } catch {
      continue;
    }
    if (`${environment}\0`.includes(marker)) killPid(Number(processId));
  }
  for (const pidFile of findFiles(sandbox.remoteHome, "server.pid")) {
    killPid(Number.parseInt(fs.readFileSync(pidFile, "utf8").trim(), 10));
  }
  let log = "";
  try {
    log = fs.readFileSync(sandbox.fakeSshLog, "utf8");
  } catch {
    return;
  }
  for (const match of log.matchAll(/^\[ssh (\d+)\] master: connected$/gm)) {
    const pid = Number(match[1]);
    let command = "";
    try {
      command = execFileSync("ps", ["-o", "command=", "-p", String(pid)], { encoding: "utf8" }).trim();
    } catch {
      continue;
    }
    // The pid may have been reused since; the fake master is exactly this sleep.
    if (command === "sleep 2147483647") killPid(pid);
  }
}

function findFiles(directory, name) {
  const matches = typeof name === "function" ? name : (candidate) => candidate === name;
  const found = [];
  let entries;
  try {
    entries = fs.readdirSync(directory, { withFileTypes: true });
  } catch {
    return found;
  }
  for (const entry of entries) {
    const full = path.join(directory, entry.name);
    if (entry.isDirectory()) found.push(...findFiles(full, name));
    else if (matches(entry.name)) found.push(full);
  }
  return found;
}

// ---------------------------------------------------------------- server process

function freePort() {
  return new Promise((resolve, reject) => {
    const probe = net.createServer();
    probe.on("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });
}

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function withTimeout(promise, milliseconds, what) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${what}: no answer within ${milliseconds}ms`)), milliseconds);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

async function startServer(sandbox, extraEnvironment) {
  const port = await freePort();
  const token = crypto.randomBytes(16).toString("hex");
  const environment = {
    ...process.env,
    ZED_WEB_TOKEN: token,
    PATH: `${sandbox.fakeBinDirectory}${path.delimiter}${process.env.PATH ?? ""}`,
    HOME: sandbox.serverHome,
    FAKE_SSH_HOME: sandbox.remoteHome,
    FAKE_SSH_LOG: sandbox.fakeSshLog,
    ...extraEnvironment,
  };
  for (const name of ["XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"]) {
    delete environment[name];
  }
  const logDescriptor = fs.openSync(sandbox.serverLog, "a");
  const child = spawn(
    path.join(sandbox.binDirectory, "zed-web-server"),
    [sandbox.root, sandbox.staticRoot, "--host", "127.0.0.1", "--port", String(port), "--allow-ssh"],
    { env: environment, stdio: ["ignore", logDescriptor, logDescriptor], cwd: sandbox.directory },
  );
  fs.closeSync(logDescriptor);
  const server = { child, port, token, exited: null };
  child.on("exit", (code, signal) => {
    server.exited = { code, signal };
  });
  cleanupActions.push(() => stopServer(server));

  const deadline = Date.now() + 30000;
  for (;;) {
    if (server.exited) {
      throw new Error(`zed-web-server exited during startup (${JSON.stringify(server.exited)}); see ${sandbox.serverLog}`);
    }
    if (await portAccepts(port)) return server;
    if (Date.now() > deadline) throw new Error(`zed-web-server did not listen on ${port} within 30s`);
    await sleep(200);
  }
}

export function portAccepts(port, host = "127.0.0.1") {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (accepted) => {
      if (settled) return;
      settled = true;
      socket.setTimeout(0);
      socket.destroy();
      resolve(accepted);
    };
    const socket = net.connect({ port, host });
    // Checked only after this promise. A connect that neither succeeds nor errors
    // would leave startServer's 30s deadline unreachable. A host that is listening
    // completes the handshake in well under a second; a refusal still retries.
    socket.setTimeout(1000);
    socket.on("connect", () => finish(true));
    socket.on("timeout", () => finish(false));
    socket.on("error", () => finish(false));
  });
}

function stopServer(server) {
  if (server.exited) return;
  try {
    server.child.kill("SIGTERM");
  } catch {
    return;
  }
  const deadline = Date.now() + 3000;
  // Synchronous on purpose: this also runs from process.on("exit").
  while (!server.exited && Date.now() < deadline) {
    try {
      process.kill(server.child.pid, 0);
    } catch {
      return;
    }
    execFileSync("sleep", ["0.1"]);
  }
  if (!server.exited) {
    try {
      server.child.kill("SIGKILL");
    } catch {
      // Already gone.
    }
  }
}

// ---------------------------------------------------------------- http + websocket

function login(port, token) {
  return withTimeout(new Promise((resolve, reject) => {
    const body = `token=${encodeURIComponent(token)}`;
    const socket = net.connect(port, "127.0.0.1", () => {
      socket.write(
        `POST /login HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\nOrigin: http://127.0.0.1:${port}\r\n` +
          `Content-Type: application/x-www-form-urlencoded\r\nContent-Length: ${body.length}\r\n` +
          `Connection: close\r\n\r\n${body}`,
      );
    });
    let buffer = "";
    socket.on("data", (chunk) => (buffer += chunk.toString("latin1")));
    socket.on("end", () => {
      const cookie = buffer.match(/set-cookie:\s*([^;]+);/i);
      if (cookie) resolve(cookie[1]);
      else reject(new Error(`login did not set a session cookie:\n${buffer.slice(0, 400)}`));
    });
    socket.on("error", reject);
  }), 10000, "POST /login");
}

class WebSocketClient {
  constructor(socket) {
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.fragments = [];
    this.fragmentKind = null;
    this.onMessage = () => {};
    this.closeResult = null;
    this.closedPromise = new Promise((resolve) => {
      this.resolveClosed = resolve;
    });
    socket.on("data", (chunk) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this.readFrames();
    });
    // A drop with no close frame is what the browser reports as 1006.
    socket.on("close", () => this.finish({ code: 1006, reason: "" }));
    socket.on("error", () => this.finish({ code: 1006, reason: "" }));
  }

  static connect(port, requestPath, cookie) {
    return withTimeout(new Promise((resolve, reject) => {
      const key = crypto.randomBytes(16).toString("base64");
      const socket = net.connect(port, "127.0.0.1", () => {
        socket.write(
          `GET ${requestPath} HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n` +
            `Sec-WebSocket-Key: ${key}\r\nSec-WebSocket-Version: 13\r\nCookie: ${cookie}\r\n\r\n`,
        );
      });
      let pending = Buffer.alloc(0);
      const onHandshake = (chunk) => {
        pending = Buffer.concat([pending, chunk]);
        const headerEnd = pending.indexOf("\r\n\r\n");
        if (headerEnd === -1) return;
        const head = pending.subarray(0, headerEnd).toString("latin1");
        socket.removeListener("data", onHandshake);
        if (!head.split("\r\n")[0].includes("101")) {
          socket.destroy();
          reject(new Error(`websocket handshake refused: ${head.slice(0, 300)}`));
          return;
        }
        const client = new WebSocketClient(socket);
        const trailing = pending.subarray(headerEnd + 4);
        if (trailing.length) socket.emit("data", trailing);
        resolve(client);
      };
      socket.on("data", onHandshake);
      socket.on("error", reject);
    }), 10000, `websocket handshake for ${requestPath.split("?")[0]}`);
  }

  finish(result) {
    if (this.closeResult) return;
    this.closeResult = result;
    this.resolveClosed(result);
  }

  readFrames() {
    for (;;) {
      if (this.buffer.length < 2) return;
      const first = this.buffer[0];
      const finalFragment = (first & 0x80) !== 0;
      const opcode = first & 0x0f;
      let length = this.buffer[1] & 0x7f;
      let offset = 2;
      if (length === 126) {
        if (this.buffer.length < 4) return;
        length = this.buffer.readUInt16BE(2);
        offset = 4;
      } else if (length === 127) {
        if (this.buffer.length < 10) return;
        length = Number(this.buffer.readBigUInt64BE(2));
        offset = 10;
      }
      if (this.buffer.length < offset + length) return;
      const payload = Buffer.from(this.buffer.subarray(offset, offset + length));
      this.buffer = this.buffer.subarray(offset + length);

      if (opcode === 0x8) {
        const code = payload.length >= 2 ? payload.readUInt16BE(0) : 1005;
        const reason = payload.length > 2 ? payload.subarray(2).toString("utf8") : "";
        this.finish({ code, reason });
        this.writeFrame(0x8, payload.subarray(0, 2));
        this.socket.end();
        return;
      }
      if (opcode === 0x9) {
        this.writeFrame(0xa, payload);
        continue;
      }
      if (opcode === 0xa) continue;
      if (opcode === 0x1 || opcode === 0x2) {
        this.fragmentKind = opcode === 0x1 ? "text" : "binary";
        this.fragments = [payload];
      } else if (opcode === 0x0) {
        this.fragments.push(payload);
      } else {
        continue;
      }
      if (finalFragment) {
        this.onMessage(this.fragmentKind, Buffer.concat(this.fragments));
        this.fragments = [];
      }
    }
  }

  writeFrame(opcode, payload) {
    const mask = crypto.randomBytes(4);
    const header = [0x80 | opcode];
    if (payload.length < 126) {
      header.push(0x80 | payload.length);
    } else if (payload.length < 0x10000) {
      header.push(0x80 | 126, payload.length >> 8, payload.length & 0xff);
    } else {
      header.push(0x80 | 127);
      const extended = Buffer.alloc(8);
      extended.writeBigUInt64BE(BigInt(payload.length));
      header.push(...extended);
    }
    const masked = Buffer.from(payload);
    for (let index = 0; index < masked.length; index++) masked[index] ^= mask[index % 4];
    if (!this.socket.destroyed) this.socket.write(Buffer.concat([Buffer.from(header), mask, masked]));
  }

  sendText(text) {
    this.writeFrame(0x1, Buffer.from(text, "utf8"));
  }

  sendBinary(bytes) {
    this.writeFrame(0x2, bytes);
  }

  close(code, reason) {
    const reasonBytes = Buffer.from(reason, "utf8");
    const payload = Buffer.alloc(2 + reasonBytes.length);
    payload.writeUInt16BE(code, 0);
    reasonBytes.copy(payload, 2);
    this.writeFrame(0x8, payload);
  }

  async waitClosed(timeoutMilliseconds) {
    const timeout = sleep(timeoutMilliseconds).then(() => null);
    return Promise.race([this.closedPromise, timeout]);
  }
}

class RpcClient {
  constructor(websocket) {
    this.websocket = websocket;
    this.nextId = 1;
    this.pending = new Map();
    this.notificationHandlers = [];
    this.notifications = [];
    websocket.onMessage = (kind, payload) => {
      if (kind !== "text") return;
      let message;
      try {
        message = JSON.parse(payload.toString("utf8"));
      } catch {
        return;
      }
      const waiting = typeof message.id === "number" ? this.pending.get(message.id) : undefined;
      if (waiting) {
        this.pending.delete(message.id);
        waiting({ result: message.result, error: message.error });
      } else if (message.method) {
        this.notifications.push(message);
        for (const handler of this.notificationHandlers) handler(message);
      }
    };
  }

  static async connect(port, cookie) {
    return new RpcClient(await WebSocketClient.connect(port, "/rpc", cookie));
  }

  onNotification(handler) {
    this.notificationHandlers.push(handler);
    return () => {
      this.notificationHandlers = this.notificationHandlers.filter((candidate) => candidate !== handler);
    };
  }

  call(method, params, timeoutMilliseconds = 30000) {
    const id = this.nextId++;
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve({ error: `no answer within ${timeoutMilliseconds}ms` });
      }, timeoutMilliseconds);
      this.pending.set(id, (answer) => {
        clearTimeout(timer);
        resolve(answer);
      });
      this.websocket.sendText(JSON.stringify({ id, method, params }));
    });
  }

  close() {
    this.websocket.close(1000, "done");
  }
}

// ---------------------------------------------------------------- protobuf by hand

function varint(value) {
  const bytes = [];
  let remaining = value;
  while (remaining >= 0x80) {
    bytes.push((remaining % 0x80) | 0x80);
    remaining = Math.floor(remaining / 0x80);
  }
  bytes.push(remaining);
  return Buffer.from(bytes);
}

function readVarint(buffer, offset) {
  let value = 0;
  let scale = 1;
  for (let index = offset; index < buffer.length; index++) {
    value += (buffer[index] & 0x7f) * scale;
    if ((buffer[index] & 0x80) === 0) return [value, index + 1];
    scale *= 0x80;
  }
  throw new Error("truncated varint");
}

// Envelope { id = 1; responding_to = 2; payload oneof by field number }. Every payload
// used here is an empty message, which is all Ping / Ack / RemoteStarted are.
function encodeEnvelope(id, payloadField) {
  return Buffer.concat([
    Buffer.from([0x08]),
    varint(id),
    varint(payloadField * 8 + 2),
    varint(0),
  ]);
}

export function decodeEnvelope(bytes) {
  const envelope = { id: 0, respondingTo: null, payloadField: null, payloadBytes: Buffer.alloc(0) };
  let offset = 0;
  while (offset < bytes.length) {
    let tag;
    [tag, offset] = readVarint(bytes, offset);
    const field = Math.floor(tag / 8);
    const wireType = tag % 8;
    if (wireType === 0) {
      let value;
      [value, offset] = readVarint(bytes, offset);
      if (field === 1) envelope.id = value;
      else if (field === 2) envelope.respondingTo = value;
    } else if (wireType === 2) {
      let length;
      [length, offset] = readVarint(bytes, offset);
      if (!Number.isSafeInteger(length) || offset + length > bytes.length) {
        throw new Error("truncated protobuf field");
      }
      if (field >= 4) {
        envelope.payloadField = field;
        envelope.payloadBytes = bytes.subarray(offset, offset + length);
      }
      offset += length;
    } else if (wireType === 1) {
      if (offset + 8 > bytes.length) throw new Error("truncated protobuf field");
      offset += 8;
    } else if (wireType === 5) {
      if (offset + 4 > bytes.length) throw new Error("truncated protobuf field");
      offset += 4;
    } else {
      throw new Error(`unsupported protobuf wire type ${wireType}`);
    }
  }
  return envelope;
}

function describeEnvelope(envelope) {
  const name = PROTO_NAME.get(envelope.payloadField) ?? `payload field ${envelope.payloadField}`;
  let detail = "";
  if (envelope.payloadField === PROTO_FIELD.error) {
    const length = envelope.payloadBytes.length;
    detail = ` ${JSON.stringify(envelope.payloadBytes.subarray(0, Math.min(length, 200)).toString("utf8"))}`;
  }
  return `${name}(id=${envelope.id}, responding_to=${envelope.respondingTo})${detail}`;
}

// The relay carries the raw stdio stream: [u32 LE length][Envelope], cut anywhere.
export function readEnvelopeFrames(buffer) {
  const envelopes = [];
  let rest = buffer;
  while (rest.length >= 4) {
    const length = rest.readUInt32LE(0);
    // Incomplete, not corrupt: the next chunk may finish a frame. A length that is
    // not a safe integer would make `4 + length` imprecise and swallow what follows.
    // Every u32 length the relay can declare is a safe integer, and a finished frame
    // has `length === rest.length - 4` when it is the only thing left.
    if (!Number.isSafeInteger(length) || length > rest.length - 4) break;
    const bytes = rest.subarray(4, 4 + length);
    rest = rest.subarray(4 + length);
    try {
      envelopes.push(decodeEnvelope(bytes));
    } catch {
      // A bad envelope is not an Ack, and its bytes are not the next frame.
    }
  }
  return { envelopes, rest };
}

// A non-empty error is not enough: submitting the wrong password also fails the connect.
export function isCancelledConnectError(error) {
  if (typeof error !== "string" || error.length === 0) return false;
  const text = error.toLowerCase();
  // askpass says "canceled"; RemoteSsh::cancel_connect says "cancelled".
  // A wrong password says "permission denied" and neither of those.
  if (text.includes("permission denied")) return false;
  return text.includes("canceled") || text.includes("cancelled");
}

class EnvelopeChannel {
  constructor(websocket) {
    this.websocket = websocket;
    this.buffer = Buffer.alloc(0);
    this.received = [];
    this.waiters = [];
    this.textMessages = 0;
    websocket.onMessage = (kind, payload) => {
      if (kind !== "binary") {
        this.textMessages++;
        return;
      }
      this.buffer = Buffer.concat([this.buffer, payload]);
      const parsed = readEnvelopeFrames(this.buffer);
      this.buffer = parsed.rest;
      this.received.push(...parsed.envelopes);
      this.wake();
    };
    websocket.closedPromise.then(() => this.wake());
  }

  wake() {
    for (const waiter of this.waiters.splice(0)) waiter();
  }

  // Sends one envelope split across two WebSocket messages, to prove the relay reassembles.
  sendSplit(envelope) {
    const frame = Buffer.concat([Buffer.alloc(4), envelope]);
    frame.writeUInt32LE(envelope.length, 0);
    const cut = Math.max(1, Math.floor(frame.length / 2));
    this.websocket.sendBinary(frame.subarray(0, cut));
    this.websocket.sendBinary(frame.subarray(cut));
  }

  send(envelope) {
    const frame = Buffer.concat([Buffer.alloc(4), envelope]);
    frame.writeUInt32LE(envelope.length, 0);
    this.websocket.sendBinary(frame);
  }

  async waitFor(predicate, timeoutMilliseconds) {
    const deadline = Date.now() + timeoutMilliseconds;
    for (;;) {
      const match = this.received.find(predicate);
      if (match) return match;
      if (this.websocket.closeResult) return null;
      const remaining = deadline - Date.now();
      if (remaining <= 0) return null;
      await new Promise((resolve) => {
        const timer = setTimeout(resolve, remaining);
        this.waiters.push(() => {
          clearTimeout(timer);
          resolve();
        });
      });
    }
  }
}

// ---------------------------------------------------------------- scenarios

function check(label, condition, detail) {
  if (!condition) throw new Error(`${label}${detail === undefined ? "" : `: ${detail}`}`);
  console.log(`  ok   ${label}`);
}

function randomHex(bytes) {
  return crypto.randomBytes(bytes).toString("hex");
}

// `port` only changes the host pool key (fake-ssh ignores -p): the server keeps a released host
// and its master for HOST_GRACE, and a connect with the same options reuses it without asking.
function connectParams(connectId, port) {
  return {
    connect_id: connectId,
    options: {
      host: { Hostname: "localhost" },
      username: null,
      port,
      password: null,
      args: [],
      port_forwards: null,
      connection_timeout: null,
      nickname: null,
      upload_binary_over_ssh: false,
    },
    client_commit: null,
  };
}

async function connectHost(rpc, onPrompt, port = null) {
  const connectId = `c-${randomHex(16)}`;
  const statuses = [];
  const stop = rpc.onNotification((message) => {
    if (message.method === `RemoteSsh::status:${connectId}`) statuses.push(message.params?.status ?? null);
    if (message.method === `RemoteSsh::prompt:${connectId}` && onPrompt) {
      const promptId = message.params?.prompt_id;
      const response = onPrompt(message.params);
      rpc.call("RemoteSsh::answer_prompt", { prompt_id: promptId, response }).then((answer) => {
        if (answer.error) console.log(`  note answer_prompt failed: ${answer.error}`);
      });
    }
  });
  const answer = await rpc.call("RemoteSsh::connect", connectParams(connectId, port), 120000);
  stop();
  return { answer, statuses, connectId };
}

async function openChannel(rpc, cookie, port, handleId, reconnect) {
  const identifier = `web-e2e-${randomHex(4)}`;
  const opened = await rpc.call("RemoteSsh::open_channel", { handle_id: handleId, identifier, reconnect });
  check(`open_channel(${identifier}, reconnect=${reconnect}) returns a channel`, !opened.error && opened.result?.channel_id && /^[0-9a-f]{64}$/.test(opened.result?.token ?? ""), JSON.stringify(opened));
  const websocket = await WebSocketClient.connect(
    port,
    `/remote/channel?channel_id=${encodeURIComponent(opened.result.channel_id)}&token=${opened.result.token}`,
    cookie,
  );
  return { websocket, channel: new EnvelopeChannel(websocket), identifier };
}

async function scenarioEnvelopesFlow(context) {
  console.log("scenario: envelopes flow through /remote/channel to a real remote_server");
  const { rpc, cookie, server, handleId } = context;
  const { websocket, channel } = await openChannel(rpc, cookie, server.port, handleId, false);

  channel.send(encodeEnvelope(1, PROTO_FIELD.remoteStarted));
  channel.sendSplit(encodeEnvelope(2, PROTO_FIELD.ping));
  const ack = await channel.waitFor(
    (envelope) => envelope.payloadField === PROTO_FIELD.ack && envelope.respondingTo === 2,
    30000,
  );
  check(
    "Ping(id=2), sent in two WebSocket messages, is answered by Ack(responding_to=2)",
    ack !== null,
    `received ${JSON.stringify(channel.received.map(describeEnvelope))}; close=${JSON.stringify(websocket.closeResult)}`,
  );
  const started = await channel.waitFor((envelope) => envelope.payloadField === PROTO_FIELD.remoteStarted, 5000);
  check(
    "the remote_server sends its own RemoteStarted",
    started !== null,
    `received ${JSON.stringify(channel.received.map(describeEnvelope))}`,
  );
  check("the relay never sends a text frame", channel.textMessages === 0, `${channel.textMessages} text frames`);

  websocket.close(1000, "client closed");
  const closed = await websocket.waitClosed(10000);
  check("a client close ends the channel", closed !== null, "the server did not answer the close");
}

async function scenarioExitCode(context, { reconnect, expectedExit }) {
  console.log(`scenario: proxy exit status arrives as close 1000 {"exit_code":${expectedExit}} (reconnect=${reconnect})`);
  const { rpc, cookie, server, handleId } = context;
  const { websocket } = await openChannel(rpc, cookie, server.port, handleId, reconnect);
  const closed = await websocket.waitClosed(30000);
  check(
    "the channel closes by itself once the proxy exits",
    closed !== null,
    "no close frame within 30s",
  );
  check(
    `close code is 1000 and reason is {"exit_code":${expectedExit}}`,
    closed.code === 1000 && closed.reason === JSON.stringify({ exit_code: expectedExit }),
    `got code=${closed.code} reason=${JSON.stringify(closed.reason)}`,
  );
}

async function scenarioRejectedIdentifier(context) {
  console.log("scenario: open_channel refuses identifiers that cannot be a socket directory name");
  const { rpc, handleId } = context;
  for (const identifier of ["web-../escape", "web-has/slash", `web-${"x".repeat(60)}`, ""]) {
    const answer = await rpc.call("RemoteSsh::open_channel", { handle_id: handleId, identifier, reconnect: false });
    check(`identifier ${JSON.stringify(identifier.slice(0, 20))} is refused`, Boolean(answer.error), JSON.stringify(answer));
  }
}

async function runConnectedGroup(sandbox, options) {
  console.log("group: connect and relay");
  const server = await startServer(sandbox, {});
  const cookie = await login(server.port, server.token);
  const rpc = await RpcClient.connect(server.port, cookie);

  const capabilities = await rpc.call("RemoteSsh::capabilities", {});
  check("capabilities: ssh is enabled", capabilities.result?.enabled === true, JSON.stringify(capabilities));
  check(
    "capabilities: the bundled commit and platform are reported",
    capabilities.result?.commit === sandbox.manifestCommit &&
      capabilities.result?.platforms?.includes(`${sandbox.platform.os}-${sandbox.platform.arch}`),
    JSON.stringify(capabilities.result),
  );

  console.log("scenario: RemoteSsh::connect to the fake host");
  const { answer, statuses } = await connectHost(rpc, null);
  check("connect succeeds", !answer.error && answer.result, answer.error ?? JSON.stringify(answer));
  const result = answer.result;
  check("connect result has host_id and handle_id", /^h-[0-9a-f]{32}$/.test(result.host_id) && /^k-[0-9a-f]{32}$/.test(result.handle_id), JSON.stringify(result));
  check(
    "connect result platform is this machine",
    result.platform?.os === sandbox.platform.os && result.platform?.arch === sandbox.platform.arch,
    JSON.stringify(result.platform),
  );
  check("connect result path_style is unix", result.path_style === "unix", JSON.stringify(result.path_style));
  check(
    "connect result carries shell, default_system_shell and a recipe",
    typeof result.shell === "string" &&
      typeof result.default_system_shell === "string" &&
      result.recipe?.destination === "localhost" &&
      Array.isArray(result.recipe?.ssh_options) &&
      result.recipe.ssh_options.some((option) => option.startsWith("ControlPath=")),
    JSON.stringify(result),
  );
  check("status notifications were sent to this connection", statuses.includes("Connecting") && statuses.at(-1) === null, JSON.stringify(statuses));

  const uploaded = findFiles(sandbox.remoteHome, `zed-remote-server-web-${sha256Of(path.join(sandbox.bundleDirectory, `remote_server-${sandbox.platform.os}-${sandbox.platform.arch}`)).slice(0, 16)}`);
  check("the bundled binary was uploaded under its content-addressed name", uploaded.length === 1, JSON.stringify(uploaded));

  const context = { rpc, cookie, server, handleId: result.handle_id };
  await scenarioRejectedIdentifier(context);
  if (options.stubRemote) {
    await scenarioExitCode(context, { reconnect: false, expectedExit: options.proxyExit });
  } else {
    await scenarioEnvelopesFlow(context);
    // `proxy --reconnect` on an identifier that never started finds no server and exits
    // ServerNotRunning, which is status 90: the exact row the spec cites.
    await scenarioExitCode(context, { reconnect: true, expectedExit: 90 });
  }

  const released = await rpc.call("RemoteSsh::release", { handle_id: result.handle_id });
  check("release succeeds", !released.error, released.error);
  rpc.close();
  stopServer(server);
}

async function runPasswordGroup(sandbox) {
  console.log("group: password prompt round trip through askpass");
  const password = `pw-${randomHex(6)}`;
  const server = await startServer(sandbox, { FAKE_SSH_PROMPT: "1", FAKE_SSH_PASSWORD: password });
  const cookie = await login(server.port, server.token);
  const rpc = await RpcClient.connect(server.port, cookie);

  console.log("scenario: the right password connects");
  let prompts = [];
  const right = await connectHost(rpc, (params) => {
    prompts.push(params.prompt);
    return password;
  });
  check("a prompt reached the requesting connection", prompts.length === 1, JSON.stringify(prompts));
  check("connect succeeds after answer_prompt", !right.answer.error && right.answer.result?.handle_id, right.answer.error ?? JSON.stringify(right.answer));
  await rpc.call("RemoteSsh::release", { handle_id: right.answer.result?.handle_id });

  console.log("scenario: connecting again within HOST_GRACE reuses the master without a prompt");
  prompts = [];
  const reused = await connectHost(rpc, (params) => {
    prompts.push(params.prompt);
    return null;
  });
  check("no prompt was sent", prompts.length === 0, JSON.stringify(prompts));
  check(
    "connect succeeds on the same host",
    !reused.answer.error && reused.answer.result?.host_id === right.answer.result?.host_id,
    JSON.stringify({ first: right.answer.result?.host_id, again: reused.answer }),
  );
  await rpc.call("RemoteSsh::release", { handle_id: reused.answer.result?.handle_id });

  console.log("scenario: a wrong password fails the connect");
  const wrong = await connectHost(rpc, () => "not-the-password", 2201);
  // The master's own refusal, not the direct login of the next command: ssh.rs must see that the
  // master exited instead of racing its exit status and failing later at 'uname -sm'.
  check(
    "connect fails with the master's Permission denied",
    typeof wrong.answer.error === "string" &&
      wrong.answer.error.includes("failed to connect: Permission denied") &&
      !wrong.answer.error.includes("uname -sm"),
    JSON.stringify(wrong.answer),
  );
  console.log(`  note error text: ${wrong.answer.error}`);

  console.log("scenario: answering null cancels the connect");
  const cancelled = await connectHost(rpc, () => null, 2202);
  check("answering null cancels instead of failing the password", isCancelledConnectError(cancelled.answer.error), JSON.stringify(cancelled.answer));
  console.log(`  note error text: ${cancelled.answer.error}`);

  rpc.close();
  stopServer(server);
}

// ---------------------------------------------------------------- main

function tail(file, lines) {
  try {
    return fs.readFileSync(file, "utf8").split("\n").slice(-lines).join("\n");
  } catch (error) {
    return `(cannot read ${file}: ${error.message})`;
  }
}

function reportFailure(sandbox, message) {
  sandboxKept = true;
  console.error(`\nFAIL: ${message}`);
  console.error(`\n--- ${sandbox.fakeSshLog} (tail) ---\n${tail(sandbox.fakeSshLog, 40)}`);
  console.error(`\n--- ${sandbox.serverLog} (tail) ---\n${tail(sandbox.serverLog, 40)}`);
  for (const logFile of findFiles(sandbox.remoteHome, (name) => name.endsWith(".log"))) {
    console.error(`\n--- ${logFile} (tail) ---\n${tail(logFile, 20)}`);
  }
  console.error(`\nsandbox kept at ${sandbox.directory}, fake remote home at ${sandbox.remoteHome}`);
}

async function main() {
  const options = parseOptions(process.argv.slice(2));
  const sandbox = createSandbox(options);
  console.log(`sandbox: ${sandbox.directory}`);
  console.log(`fake remote home: ${sandbox.remoteHome}`);
  console.log(`remote_server: ${sandbox.remoteServerDescription}`);
  // A backstop only: every wait below has its own, shorter bound.
  setTimeout(() => {
    reportFailure(sandbox, "overall 5 minute limit exceeded");
    process.exit(1);
  }, 5 * 60 * 1000);
  try {
    await runConnectedGroup(sandbox, options);
    if (!options.skipPassword) await runPasswordGroup(sandbox);
    console.log("\nPASS");
    return 0;
  } catch (error) {
    reportFailure(sandbox, error.message);
    return 1;
  }
}

// Exit explicitly: after a failure the server child, the /rpc socket and a channel socket are
// still open and would keep node running until something killed it.
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then(
    (status) => process.exit(status),
    (error) => {
      console.error(error);
      process.exit(1);
    },
  );
}

