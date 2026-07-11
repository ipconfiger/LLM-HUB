#!/usr/bin/env node
// pack-npm.mjs — build the current-host binary and stage it at
// dist/npm/llm-hub/bin/llm-hub[.exe] so the launcher can be tested
// locally without a Release download.
//
// Usage:
//   node scripts/pack-npm.mjs
//
// Environment:
//   CARGO_TARGET_DIR  target directory (default: "target")

import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, existsSync, rmSync, chmodSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, "..");
const PACKAGE_BIN_DIR = join(REPO_ROOT, "dist", "npm", "llm-hub", "bin");

const cargoTargetDir = process.env.CARGO_TARGET_DIR ?? "target";
const binName = process.platform === "win32" ? "llm-hub.exe" : "llm-hub";

// ---- build ---------------------------------------------------------------
const buildArgs = ["build", "--release"];
console.log(`>> cargo ${buildArgs.join(" ")}`);
execFileSync("cargo", buildArgs, { cwd: REPO_ROOT, stdio: "inherit" });

// ---- locate built binary -------------------------------------------------
const binPath = join(cargoTargetDir, "release", binName);
if (!existsSync(binPath)) {
  console.error(`Build succeeded but binary not found at: ${binPath}`);
  process.exit(1);
}

// ---- stage into package bin/ --------------------------------------------
const outBinPath = join(PACKAGE_BIN_DIR, binName);
mkdirSync(PACKAGE_BIN_DIR, { recursive: true });
if (existsSync(outBinPath)) rmSync(outBinPath);
copyFileSync(binPath, outBinPath);

if (process.platform !== "win32") {
  chmodSync(outBinPath, 0o755);
}

console.log(`>> staged binary -> ${outBinPath}`);
