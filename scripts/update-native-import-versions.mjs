#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const repoRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname), "..");
const manifestPath = path.join(repoRoot, "docs/specs/native-session-import-versions.toml");

const args = new Map();
for (let index = 2; index < process.argv.length; index += 1) {
  const name = process.argv[index];
  if (!name.startsWith("--")) {
    throw new Error(`unexpected argument: ${name}`);
  }
  const value = process.argv[index + 1];
  if (value === undefined || value.startsWith("--")) {
    throw new Error(`${name} requires a value`);
  }
  args.set(name.slice(2), value);
  index += 1;
}

const latest = [
  toolEntry("claude", args.get("claude")),
  toolEntry("opencode", args.get("opencode")),
  toolEntry("codex", args.get("codex")),
  toolEntry("copilot", args.get("copilot")),
].filter(Boolean);

if (latest.length === 0) {
  console.log("no tool versions supplied; manifest unchanged");
  process.exit(0);
}

let manifest = fs.readFileSync(manifestPath, "utf8");
const additions = [];
const newEntries = [];

for (const entry of latest) {
  const currentStatus = toolVersionStatus(manifest, entry.tool, entry.version);
  if (currentStatus === entry.status) {
    continue;
  }
  if (currentStatus !== null) {
    manifest = replaceToolVersionEntry(manifest, entry);
    additions.push(renderEntry(entry));
    continue;
  }
  additions.push(renderEntry(entry));
  newEntries.push(renderEntry(entry));
}

if (additions.length === 0) {
  console.log("native import version manifest already lists latest detected versions");
  process.exit(0);
}

if (newEntries.length > 0) {
  manifest = `${manifest.trimEnd()}\n\n${newEntries.join("\n").trimEnd()}\n`;
} else {
  manifest = `${manifest.trimEnd()}\n`;
}
fs.writeFileSync(manifestPath, manifest);

for (const addition of additions) {
  process.stdout.write(addition);
}

function toolEntry(tool, rawVersion) {
  const version = normalizeVersion(rawVersion);
  if (!version) {
    return null;
  }
  if (tool === "claude") {
    return {
      tool,
      version,
      source: "ci-sessiongator-roundtrip",
      status: "probe-passed",
      store: "jsonl-projects",
      fixtureRoot: "fixtures/native-import/claude/2.1.199",
      notes:
        "CI latest-tool probe passed dry-run and isolated target-store writes against the same projects JSONL layout as 2.1.199.",
    };
  }
  if (tool === "opencode") {
    return {
      tool,
      version,
      source: "ci-native-harness-export",
      status: "target-supported",
      store: "sqlite",
      fixtureRoot: "fixtures/native-import/opencode/1.17.13",
      notes:
        "CI initialized the isolated SQLite store through opencode, then opencode exported the sessiongator-written session successfully.",
    };
  }
  if (tool === "codex") {
    return {
      tool,
      version,
      source: "ci-sessiongator-roundtrip",
      status: "probe-passed",
      store: "rollout-jsonl",
      fixtureRoot: "fixtures/native-import/codex/0.142.5",
      notes:
        "CI latest-tool probe passed isolated rollout JSONL writes and readback through the Codex adapter.",
    };
  }
  if (tool === "copilot") {
    return {
      tool,
      version,
      source: "ci-native-harness-resume",
      status: "target-supported",
      store: "session-store-sqlite-plus-session-state",
      fixtureRoot: "fixtures/native-import/copilot/1.0.68",
      notes:
        "CI confirmed copilot --resume discovers and parses the isolated session without authentication or a model request.",
    };
  }
  throw new Error(`unsupported tool: ${tool}`);
}

function normalizeVersion(rawVersion) {
  if (!rawVersion || rawVersion === "null") {
    return null;
  }
  const firstToken = rawVersion.trim().split(" ").filter(Boolean)[0] ?? "";
  const version = firstToken.startsWith("v") ? firstToken.slice(1) : firstToken;
  return version.length > 0 ? version : null;
}

function toolVersionStatus(manifest, tool, version) {
  const blocks = manifest.split("[[tools]]").slice(1);
  const block = blocks.find((block) => {
    return (
      tomlValue(block, "tool") === tool &&
      tomlValue(block, "version") === version
    );
  });
  return block ? tomlValue(block, "status") : null;
}

function replaceToolVersionEntry(manifest, entry) {
  const marker = "[[tools]]";
  const blocks = manifest.split(marker);
  const index = blocks.findIndex((block, blockIndex) => {
    return (
      blockIndex > 0 &&
      tomlValue(block, "tool") === entry.tool &&
      tomlValue(block, "version") === entry.version
    );
  });
  if (index < 0) {
    throw new Error(`missing manifest entry for ${entry.tool} ${entry.version}`);
  }
  blocks[index] = `\n${renderEntry(entry).slice(marker.length).trim()}\n\n`;
  return blocks.join(marker).trimEnd();
}

function tomlValue(block, key) {
  const prefix = `${key} = `;
  const line = block
    .split("\n")
    .map((line) => line.trim())
    .find((line) => line.startsWith(prefix));
  if (!line) {
    return null;
  }
  const value = line.slice(prefix.length).trim();
  if (!value.startsWith('"')) {
    return null;
  }
  return value.slice(1).split('"')[0];
}

function renderEntry(entry) {
  return `[[tools]]
tool = "${entry.tool}"
version = "${entry.version}"
status = "${entry.status}"
source = "${entry.source}"
store = "${entry.store}"
fixture_root = "${entry.fixtureRoot}"
notes = "${entry.notes}"
`;
}
