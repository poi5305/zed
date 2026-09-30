import assert from "node:assert/strict";
import { test } from "node:test";
import {
  decodeEnvelope,
  isCancelledConnectError,
  portAccepts,
  readEnvelopeFrames,
} from "./remote-e2e.mjs";

function frame(payload) {
  const header = Buffer.alloc(4);
  header.writeUInt32LE(payload.length, 0);
  return Buffer.concat([header, payload]);
}

function shape(envelope) {
  return {
    id: envelope.id,
    payloadField: envelope.payloadField,
    respondingTo: envelope.respondingTo,
  };
}

test("a truncated ack is not success and does not hide the next frame", { timeout: 5000 }, () => {
  // responding_to = 2, Ack (field 5) claiming 40 bytes that are not in the frame.
  const truncated = Buffer.from([0x10, 0x02, 0x2a, 40]);
  // A complete empty Ack for id 1, responding_to 2.
  const complete = Buffer.from([0x08, 0x01, 0x10, 0x02, 0x2a, 0x00]);
  const parsed = readEnvelopeFrames(Buffer.concat([frame(truncated), frame(complete)]));
  assert.deepEqual(parsed.envelopes.map(shape), [
    { id: 1, payloadField: 5, respondingTo: 2 },
  ]);

  const alone = decodeEnvelope(complete);
  assert.deepEqual(shape(alone), { id: 1, payloadField: 5, respondingTo: 2 });
});

test("cancelling is not the same error as a wrong password", { timeout: 5000 }, () => {
  assert.equal(
    isCancelledConnectError("failed to connect: Permission denied (publickey,password)."),
    false,
  );
  assert.equal(isCancelledConnectError("Failed to connect to host: SSH connection canceled"), true);
  assert.equal(isCancelledConnectError("connect c-1 was cancelled"), true);
  assert.equal(isCancelledConnectError(""), false);
  assert.equal(isCancelledConnectError(null), false);
});

test("portAccepts settles when the host never answers", { timeout: 5000 }, async () => {
  const result = await Promise.race([
    portAccepts(9, "192.0.2.1").then((accepted) => accepted),
    new Promise((resolve) => setTimeout(() => resolve("pending"), 2000)),
  ]);
  assert.equal(result, false);
});
