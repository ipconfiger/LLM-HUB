#!/usr/bin/env node
"use strict";

const { spawn } = require("node:child_process");
const { existsSync } = require("node:fs");
const { join } = require("node:path");

const platform = process.platform;
const arch = process.arch;

const binName = platform === "win32" ? "llm-hub.exe" : "llm-hub";

const subpackageMap = {
  darwin: { arm64: "darwin-arm64", x64: "darwin-x64" },
  linux: { x64: "linux-x64", arm64: "linux-arm64" },
  win32: { x64: "win32-x64" },
};

const archMap = (subpackageMap[platform] || {});
const subpkgKey = archMap[arch];

if (!subpkgKey) {
  const supported = Object.values(subpackageMap).flatMap(Object.values).join(", ");
  process.stderr.write(
    `llm-hub: no prebuilt binary for ${platform}/${arch}.\n` +
    `Supported: ${supported}\n` +
    `You can build from source: https://github.com/ipconfiger/LLM-HUB\n`
  );
  process.exit(1);
}

const subpackage = `@ipconfiger/llm-hub-${subpkgKey}`;

// Resolve the prebuilt binary for the current platform.
//   1. try require.resolve (works when installed as a dependency / npm pkg)
//   2. fall back to a relative path (local dev without npm install)
let binPath;
try {
  const manifestPath = require.resolve(`${subpackage}/package.json`);
  binPath = join(manifestPath, "..", "bin", binName);
} catch {
  binPath = join(__dirname, "..", "..", subpackage, "bin", binName);
}

if (!existsSync(binPath)) {
  process.stderr.write(
    `llm-hub: prebuilt binary not found for ${platform}/${arch}.\n` +
    `Expected at: ${binPath}\n` +
    `Try reinstalling this package, or build from source: https://github.com/ipconfiger/LLM-HUB\n`
  );
  process.exit(1);
}

const child = spawn(binPath, process.argv.slice(2), { stdio: "inherit" });

child.on("error", (err) => {
  process.stderr.write(`llm-hub: failed to launch binary: ${err.message}\n`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});
