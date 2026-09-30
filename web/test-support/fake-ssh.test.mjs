import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import path from "node:path";
import { test } from "node:test";

const fakeSsh = path.join(import.meta.dirname, "fake-ssh");

test("ssh -G names a missing control socket so -O check runs", { timeout: 5000 }, () => {
  const stdout = execFileSync(fakeSsh, ["-G", "devbox"], {
    encoding: "utf8",
    timeout: 3000,
  });
  const controlPath = stdout
    .split("\n")
    .find((line) => line.startsWith("controlpath "));
  assert.equal(controlPath, "controlpath /nonexistent");

  let exitCode = 0;
  try {
    execFileSync(
      fakeSsh,
      ["-O", "check", "-o", "ControlPath=/nonexistent", "devbox"],
      { encoding: "utf8", timeout: 3000 },
    );
  } catch (error) {
    exitCode = error.status;
  }
  assert.equal(exitCode, 255);
});
