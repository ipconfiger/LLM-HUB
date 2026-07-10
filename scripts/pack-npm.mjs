#!/usr/bin/env node
// pack-npm.mjs — build the host binary and stage it into the correct npm
// platform subpackage under dist/npm/@ipconfiger/. Useful for local testing
// and `npm pack` dry-runs before publishing.
//
// Usage:
//   node scripts/pack-npm.mjs              # auto-detect host platform
//   node scripts/pack-npm.mjs --target aarch64-unknown-linux-musl
//
// Environment:
//   CARGO_TARGET_DIR  target directory (default: "target")

import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, existsSync, rmSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, "..");
const NPM_DIST = join(REPO_ROOT, "dist", "npm");

// ---- parse args ----------------------------------------------------------
const args = process.argv.slice(2);
let targetRustTriple = null;
for (let i = 0; i < args.length; i++) {
  if (args[i] === "--target" && i + 1 < args.length) {
    targetRustTriple = args[i + 1];
    i++;
  }
}

// ---- resolve which subpackage this build targets --------------------------
const HOST = { platform: process.platform, arch: process.arch };

const PLATFORM_MAP = {
  darwin: { arm64: "darwin-arm64", x64: "darwin-x64" },
  linux: { x64: "linux-x64", arm64: "linux-arm64" },
  win32: { x64: "win32-x64" },
};

const TARGET_TO_SUBPKG = {
  "aarch64-apple-darwin": "darwin-arm64",
  "x86_64-apple-darwin": "darwin-x64",
  "x86_64-unknown-linux-musl": "linux-x64",
  "aarch64-unknown-linux-musl": "linux-arm64",
  "x86_64-pc-windows-msvc": "win32-x64",
};

let subpkgKey;
if (targetRustTriple) {
  subpkgKey = TARGET_TO_SUBPKG[targetRustTriple];
  if (!subpkgKey) {
    console.error(`Unknown --target triple: ${targetRustTriple}`);
    console.error(`Valid: ${Object.keys(TARGET_TO_SUBPKG).join(", ")}`);
    process.exit(1);
  }
} else {
  subpkgKey = (PLATFORM_MAP[HOST.platform] || {})[HOST.arch];
  if (!subpkgKey) {
    console.error(`No prebuilt target for ${HOST.platform}/${HOST.arch}`);
    console.error("Pass --target <rust-triple> to cross-build.");
    process.exit(1);
  }
}

// ---- locate cargo target dir ---------------------------------------------
const cargoTargetDir = process.env.CARGO_TARGET_DIR ?? "target";

// ---- build ---------------------------------------------------------------
const buildArgs = ["build", "--release"];
if (targetRustTriple) buildArgs.push("--target", targetRustTriple);
console.log(`>> cargo ${buildArgs.join(" ")}`);
execFileSync("cargo", buildArgs, { cwd: REPO_ROOT, stdio: "inherit" });

// ---- locate the built binary ---------------------------------------------
const releaseDir = targetRustTriple
  ? join(cargoTargetDir, targetRustTriple, "release")
  : join(cargoTargetDir, "release");
const binName = HOST.platform === "win32" && !targetRustTriple
  ? "llm-hub.exe"
  : targetRustTriple?.includes("windows")
    ? "llm-hub.exe"
    : "llm-hub";
const binPath = join(releaseDir, binName);

if (!existsSync(binPath)) {
  console.error(`Build succeeded but binary not found at: ${binPath}`);
  process.exit(1);
}

// ---- stage into subpackage -----------------------------------------------
const subpkgDir = join(NPM_DIST, "@ipconfiger", `llm-hub-${subpkgKey}`);
const outBinDir = join(subpkgDir, "bin");
const outBinName = subpkgKey.startsWith("win32") ? "llm-hub.exe" : "llm-hub";
const outBinPath = join(outBinDir, outBinName);

mkdirSync(outBinDir, { recursive: true });
if (existsSync(outBinPath)) rmSync(outBinPath);
copyFileSync(binPath, outBinPath);

console.log(`>> staged binary -> ${outBinPath}`);
console.log(`   subpackage: @ipconfiger/llm-hub-${subpkgKey}`);
