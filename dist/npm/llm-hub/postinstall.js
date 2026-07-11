#!/usr/bin/env node
"use strict";

const https = require("node:https");
const fs = require("node:fs");
const path = require("node:path");

const REPO = "ipconfiger/LLM-HUB";

/**
 * Map (platform, arch, variant) to the Release asset name.
 * Returns null for unsupported platforms.
 */
function assetName(platform, arch, variant) {
  const v = (variant || "").toLowerCase();
  switch (platform) {
    case "darwin":
      if (arch === "arm64") return "llm-hub-darwin-arm64";
      if (arch === "x64") return "llm-hub-darwin-x64";
      break;
    case "linux":
      if (arch === "x64") return v === "gnu" ? "llm-hub-linux-x64-gnu" : "llm-hub-linux-x64";
      if (arch === "arm64") return v === "gnu" ? "llm-hub-linux-arm64-gnu" : "llm-hub-linux-arm64";
      break;
    case "win32":
      if (arch === "x64") return "llm-hub-win32-x64.exe";
      break;
  }
  return null;
}

function binName(platform) {
  return platform === "win32" ? "llm-hub.exe" : "llm-hub";
}

function download(url, dest) {
  return new Promise((resolve, reject) => {
    const req = https.get(url, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        download(res.headers.location, dest).then(resolve, reject);
        return;
      }
      if (res.statusCode !== 200) {
        res.resume();
        reject(new Error(`HTTP ${res.statusCode} for ${url}`));
        return;
      }
      const stream = fs.createWriteStream(dest);
      res.pipe(stream);
      stream.on("finish", () => stream.close(resolve));
      stream.on("error", reject);
    });
    req.on("error", reject);
    req.setTimeout(120000, () => {
      req.destroy(new Error("download timed out (120s)"));
    });
  });
}

async function main() {
  const platform = process.platform;
  const arch = process.arch;
  const variant = process.env.LLM_HUB_VARIANT || "";

  const asset = assetName(platform, arch, variant);
  if (!asset) {
    process.stderr.write(
      `llm-hub: unsupported platform ${platform}/${arch}.\n` +
      `Build from source: https://github.com/${REPO}\n`
    );
    process.exit(1);
  }

  const outBin = binName(platform);
  const binDir = path.join(__dirname, "bin");
  const binPath = path.join(binDir, outBin);

  // Skip re-download if binary already present and non-empty (unless forced)
  if (
    fs.existsSync(binPath) &&
    fs.statSync(binPath).size > 0 &&
    process.env.LLM_HUB_FORCE_DOWNLOAD !== "1"
  ) {
    return;
  }

  const pkg = JSON.parse(
    fs.readFileSync(path.join(__dirname, "package.json"), "utf8")
  );
  const version = pkg.version;

  const url = `https://github.com/${REPO}/releases/download/v${version}/${asset}`;

  fs.mkdirSync(binDir, { recursive: true });

  try {
    await download(url, binPath);
  } catch (err) {
    process.stderr.write(
      `llm-hub: failed to download binary: ${err.message}\n` +
      `URL: ${url}\n` +
      `Manual download: https://github.com/${REPO}/releases\n`
    );
    process.exit(1);
  }

  if (platform !== "win32") {
    fs.chmodSync(binPath, 0o755);
  }
}

// Run only when invoked directly (not when required for testing)
if (require.main === module) {
  main().catch((err) => {
    process.stderr.write(`llm-hub: postinstall error: ${err.message}\n`);
    process.exit(1);
  });
}

module.exports = { assetName, binName };
