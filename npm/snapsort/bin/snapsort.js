#!/usr/bin/env node
"use strict";

// Launcher for the snapsort CLI. The native binary ships in a per-platform
// package (e.g. @snapsort/darwin-arm64) that npm installs as an optional
// dependency, picking only the one matching this machine's os/cpu. This file
// finds that binary and runs it with the same arguments and stdio.

const { spawn } = require("node:child_process");
const fs = require("node:fs");

const platforms = require("../platforms.json");

function fail(message) {
  process.stderr.write(`snapsort: ${message}\n`);
  process.exit(1);
}

function resolveBinary() {
  if (process.env.SNAPSORT_BINARY) {
    return process.env.SNAPSORT_BINARY;
  }

  const key = `${process.platform}-${process.arch}`;
  const entry = platforms[key];
  if (!entry) {
    fail(
      `no prebuilt binary for ${key}. Supported: ${Object.keys(platforms).join(", ")}.\n` +
        "You can build it from source with cargo instead (see the snapsort README).",
    );
  }

  const exe = process.platform === "win32" ? "snapsort.exe" : "snapsort";
  try {
    return require.resolve(`${entry.package}/bin/${exe}`);
  } catch {
    fail(
      `the platform package ${entry.package} is not installed.\n` +
        "It is an optional dependency of snapsort, so it is skipped when optional\n" +
        "dependencies are disabled (--omit=optional / --no-optional). Reinstall with:\n" +
        "  npm install -g snapsort",
    );
  }
}

function ensureExecutable(file) {
  if (process.platform === "win32") return;
  try {
    fs.accessSync(file, fs.constants.X_OK);
  } catch {
    try {
      fs.chmodSync(file, 0o755);
    } catch {
      // Read-only install location; spawn will report the real error.
    }
  }
}

const binary = resolveBinary();
ensureExecutable(binary);

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

// Ctrl-C is delivered by the terminal to the whole process group, so the
// binary already receives it (first press: finish in-flight images, second:
// quit). Ignore it here rather than dying first or forwarding a duplicate.
process.on("SIGINT", () => {});
process.on("SIGHUP", () => {});
process.on("SIGTERM", () => child.kill("SIGTERM"));

child.on("error", (err) => fail(`failed to start ${binary}: ${err.message}`));
child.on("exit", (code, signal) => {
  if (signal) {
    // Mirror the child's death by signal so the shell sees the same status.
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
