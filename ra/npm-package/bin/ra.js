#!/usr/bin/env node

const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

function main() {
  const binPath = path.join(__dirname, "ra");
  if (!fs.existsSync(binPath)) {
    console.error(
      "ra binary is not installed yet.\n" +
        "Try reinstalling: npm i -g react-agent-cli\n" +
        "If this is a CI environment, ensure postinstall scripts are enabled."
    );
    process.exit(1);
  }

  const result = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });
  if (result.error) {
    console.error(result.error.message);
    process.exit(1);
  }
  process.exit(result.status ?? 0);
}

main();


