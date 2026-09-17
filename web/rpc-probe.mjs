#!/usr/bin/env node
// Calls zed-web-server's /rpc methods directly, without a browser.
//
// The port's three verification layers -- `cargo check`, the link step, and
// loading it in a browser -- all answer questions about the *client*. When a
// panel is empty in the browser there is no layer that says whether the server
// would have answered, so the same symptom covers "the RPC is unimplemented",
// "the client never called it" and "the client called it and dropped the
// answer". This separates them: it speaks the same wire protocol the wasm
// client does, so a green run here means the remaining fault is client-side.
//
// Node's global WebSocket cannot send a Cookie header, so the handshake is
// done by hand over a TCP socket. No dependencies, deliberately.
//
//   ZED_WEB_TOKEN=<token> node web/rpc-probe.mjs \
//     --port 8099 \
//     'Home::dirs' '{}' \
//     'ClaudeSessions::list_sessions' '{"project_root":null}'
//
// With no method arguments it runs a default sweep covering the three panels
// docs/web-zed-plan.md §6 adds, which is the question this was written for.

import net from "node:net";
import crypto from "node:crypto";

const DEFAULT_SWEEP = [
  ["Home::dirs", {}],
  ["Fs::root", {}],
  ["Workspace::ui_state", {}],
  // project_manager reads its list through RemoteFs rather than a panel RPC.
  ["Fs::load", { path: null }],
  // tmux_sessions has no RPC of its own on the web: the panel's "local" branch
  // shells out, and on wasm `util::command::Command` is smol_wasm's, which
  // routes here. This is the exact argv `remote::tmux_sessions` builds.
  [
    "Process::output",
    {
      program: "tmux",
      args: ["list-sessions", "-F", "#{session_name}\t#{?session_attached,1,0}\t#{session_windows}"],
      env: {},
      cwd: null,
      stdin_pipe: false,
      stdout_pipe: true,
      stderr_pipe: true,
    },
  ],
  ["ClaudeSessions::list_sessions", { project_root: null }],
  [
    "ShellEnv::capture",
    {
      shell_path: "/bin/sh",
      args: [],
      directory: null,
    },
  ],
];

function parseArguments(argv) {
  let port = 8099;
  let host = "127.0.0.1";
  const probes = [];
  const rest = [];
  for (let index = 0; index < argv.length; index++) {
    if (argv[index] === "--port") port = Number(argv[++index]);
    else if (argv[index] === "--host") host = argv[++index];
    else rest.push(argv[index]);
  }
  for (let index = 0; index < rest.length; index += 2) {
    const method = rest[index];
    const params = rest[index + 1] === undefined ? {} : JSON.parse(rest[index + 1]);
    probes.push([method, params]);
  }
  return { host, port, probes };
}

function login(host, port, token) {
  return new Promise((resolve, reject) => {
    const body = `token=${encodeURIComponent(token)}`;
    const socket = net.connect(port, host, () => {
      socket.write(
        `POST /login HTTP/1.1\r\nHost: ${host}:${port}\r\nOrigin: http://${host}:${port}\r\n` +
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
  });
}

class RpcSocket {
  constructor(socket) {
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.nextId = 1;
    this.pending = new Map();
    this.notifications = [];
    socket.on("data", (chunk) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this.readFrames();
    });
  }

  static connect(host, port, cookie) {
    return new Promise((resolve, reject) => {
      const key = crypto.randomBytes(16).toString("base64");
      const socket = net.connect(port, host, () => {
        socket.write(
          `GET /rpc HTTP/1.1\r\nHost: ${host}:${port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n` +
            `Sec-WebSocket-Key: ${key}\r\nSec-WebSocket-Version: 13\r\nCookie: ${cookie}\r\n\r\n`,
        );
      });
      const onHandshake = (chunk) => {
        const text = chunk.toString("latin1");
        const headerEnd = text.indexOf("\r\n\r\n");
        if (headerEnd === -1) return;
        if (!text.split("\r\n")[0].includes("101")) {
          reject(new Error(`handshake refused: ${text.slice(0, 300)}`));
          return;
        }
        socket.removeListener("data", onHandshake);
        const client = new RpcSocket(socket);
        const trailing = chunk.subarray(Buffer.byteLength(text.slice(0, headerEnd + 4), "latin1"));
        if (trailing.length) socket.emit("data", trailing);
        resolve(client);
      };
      socket.on("data", onHandshake);
      socket.on("error", reject);
    });
  }

  readFrames() {
    for (;;) {
      if (this.buffer.length < 2) return;
      const opcode = this.buffer[0] & 0x0f;
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
      const payload = this.buffer.subarray(offset, offset + length);
      this.buffer = this.buffer.subarray(offset + length);
      if (opcode === 0x8) {
        this.socket.end();
        return;
      }
      if (opcode !== 0x1 && opcode !== 0x2) continue;
      let message;
      try {
        message = JSON.parse(payload.toString("utf8"));
      } catch {
        continue;
      }
      const waiting = typeof message.id === "number" ? this.pending.get(message.id) : undefined;
      if (waiting) {
        this.pending.delete(message.id);
        waiting({ result: message.result, error: message.error });
      } else if (message.method) {
        this.notifications.push({ method: message.method, params: message.params });
      }
    }
  }

  send(text) {
    const payload = Buffer.from(text, "utf8");
    const mask = crypto.randomBytes(4);
    const header = [0x81];
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
    this.socket.write(Buffer.concat([Buffer.from(header), mask, masked]));
  }

  call(method, params, timeoutMilliseconds = 25000) {
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
      this.send(JSON.stringify({ id, method, params }));
    });
  }

  close() {
    this.socket.end();
  }
}

async function main() {
  const token = process.env.ZED_WEB_TOKEN;
  if (!token) {
    console.error("ZED_WEB_TOKEN is not set; it must match the token the server was started with");
    process.exit(2);
  }
  const { host, port, probes } = parseArguments(process.argv.slice(2));
  const sweep = probes.length ? probes : DEFAULT_SWEEP;

  const cookie = await login(host, port, token);
  const client = await RpcSocket.connect(host, port, cookie);

  // `Fs::load` in the default sweep has no path until the server says where
  // home is, because project_manager reads `<config>/projects.json`.
  // `ShellEnv::capture` needs the served project directory, not ~/.
  const home = await client.call("Home::dirs", {});
  const root = await client.call("Fs::root", {});
  let failures = 0;
  for (const [method, params] of sweep) {
    const resolved =
      method === "Fs::load" && params.path === null
        ? { path: `${home.result?.config ?? ""}/projects.json` }
        : method === "ShellEnv::capture" && params.directory === null
          ? { ...params, directory: root.result }
          : params;
    const answer = await client.call(method, resolved);
    if (answer.error) {
      failures++;
      console.log(`FAIL ${method}\n       ${answer.error}`);
    } else {
      const rendered = JSON.stringify(answer.result) ?? "null";
      console.log(`ok   ${method}\n       ${rendered.length > 300 ? rendered.slice(0, 300) + " …" : rendered}`);
    }
  }
  client.close();

  console.log(`\n${sweep.length} probes, ${failures} failures`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
