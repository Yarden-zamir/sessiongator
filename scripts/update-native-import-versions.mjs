#!/usr/bin/env node
// Record tool versions in docs/specs/native-session-import-versions.toml.
//
//   node scripts/update-native-import-versions.mjs --status probe-passed \
//     --source ci-probe --notes "..." claude opencode codex copilot
//
// A bare tool name records its installed version (`<tool> --version`);
// `tool=1.2.3` records that version. An existing entry only moves up
// (probe-passed -> verified), never down. Prints each added or changed entry.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const RANK = { "probe-passed": 1, verified: 2 };
const TOOLS = ["claude", "opencode", "codex", "copilot"];
const manifestPath = path.join(
  path.dirname(new URL(import.meta.url).pathname),
  "../docs/specs/native-session-import-versions.toml",
);

const options = { status: null, source: null, notes: "" };
const requested = [];
const argv = process.argv.slice(2);
for (let index = 0; index < argv.length; index += 1) {
  const arg = argv[index];
  if (arg.startsWith("--")) {
    const key = arg.slice(2);
    if (!(key in options) || argv[index + 1] === undefined) {
      throw new Error(`unknown option or missing value: ${arg}`);
    }
    options[key] = argv[index + 1];
    index += 1;
  } else {
    requested.push(arg);
  }
}
if (!(options.status in RANK)) {
  throw new Error(`--status must be one of: ${Object.keys(RANK).join(", ")}`);
}
if (!options.source) {
  throw new Error("--source is required");
}

const blockSeparator = "\n[[tools]]\n";
let manifest = fs.readFileSync(manifestPath, "utf8");
const [header, ...blocks] = manifest.split(blockSeparator);
const changes = [];

for (const item of requested) {
  const [tool, explicit] = item.split("=");
  if (!TOOLS.includes(tool)) {
    throw new Error(`unknown tool: ${tool}`);
  }
  const version = explicit ?? installedVersion(tool);
  if (!version) {
    throw new Error(`cannot detect the ${tool} version`);
  }
  const entry = renderEntry({ tool, version, ...options });
  const index = blocks.findIndex(
    (block) => value(block, "tool") === tool && value(block, "version") === version,
  );
  if (index < 0) {
    blocks.push(entry);
    changes.push(entry);
  } else if (RANK[options.status] > (RANK[value(blocks[index], "status")] ?? 0)) {
    blocks[index] = entry;
    changes.push(entry);
  }
}

if (changes.length === 0) {
  console.error("manifest already lists these versions with this status or higher");
  process.exit(0);
}
manifest = [header, ...blocks.map((block) => block.trimEnd() + "\n")].join(blockSeparator);
fs.writeFileSync(manifestPath, manifest);
for (const change of changes) {
  process.stdout.write(`[[tools]]\n${change}\n`);
}

/// First dotted version in `<tool> --version` output, without a `v` prefix.
function installedVersion(tool) {
  const result = spawnSync(tool, ["--version"], { encoding: "utf8" });
  if (result.status !== 0) {
    return null;
  }
  for (const token of result.stdout.split(/\s+/)) {
    const candidate = token.replace(/^v/, "").replace(/\.$/, "");
    if (/^\d+(\.\d+)+$/.test(candidate)) {
      return candidate;
    }
  }
  return null;
}

function value(block, key) {
  const line = block.split("\n").find((line) => line.startsWith(`${key} = "`));
  return line ? line.slice(key.length + 4).split('"')[0] : null;
}

function renderEntry({ tool, version, status, source, notes }) {
  const quote = (text) => JSON.stringify(text);
  return [
    `tool = ${quote(tool)}`,
    `version = ${quote(version)}`,
    `status = ${quote(status)}`,
    `source = ${quote(source)}`,
    `notes = ${quote(notes)}`,
  ].join("\n") + "\n";
}
