#!/usr/bin/env node
"use strict";

// Runs the binary from the platform package npm installed for this machine
// (@agent-walker/<platform>-<arch>, matched by its os/cpu/libc fields).
// Installed as bin/agent-walker.js and bin/agw.js, which run the same
// binary: the Release's agw is an identical build under another name.

const { spawnSync } = require("child_process");
const path = require("path");

const command = path.basename(__filename, ".js");
const target = `${process.platform}-${process.arch}`;
const executable = process.platform === "win32" ? "agent-walker.exe" : "agent-walker";

function fail(message) {
  console.error(`${command}: ${message}`);
  process.exit(1);
}

let binary;
try {
  binary = require.resolve(`@agent-walker/${target}/bin/${executable}`);
} catch {
  const published = Object.keys(require("../package.json").optionalDependencies)
    .map((name) => name.split("/")[1])
    .join(", ");
  fail(
    `no binary for ${target} is installed. Builds exist for ${published} ` +
      "(Linux needs glibc). Installs with --omit=optional or --no-optional " +
      "skip them; reinstall without that flag.",
  );
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  fail(result.error.message);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
