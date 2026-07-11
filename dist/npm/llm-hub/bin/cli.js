#!/usr/bin/env node
"use strict";

const { spawn } = require("node:child_process");
const { existsSync } = require("node:fs");
const { join } = require("node:path");

const binName = process.platform === "win32" ? "llm-hub.exe" : "llm-hub";
const binPath = join(__dirname, binName);

if (!existsSync(binPath)) {
  process.stderr.write(
    "llm-hub: 原生二进制未就绪;请重新安装,或从 " +
    "https://github.com/ipconfiger/LLM-HUB/releases 手动下载\n"
  );
  process.exit(127);
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
