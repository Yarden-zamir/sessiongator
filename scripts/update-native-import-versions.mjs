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
].filter(Boolean);

if (latest.length === 0) {
  console.log("no tool versions supplied; manifest unchanged");
  process.exit(0);
}

let manifest = fs.readFileSync(manifestPath, "utf8");
const additions = [];

for (const entry of latest) {
  if (hasExactToolVersion(manifest, entry.tool, entry.version)) {
    continue;
  }
  additions.push(renderEntry(entry));
}

if (additions.length === 0) {
  console.log("native import version manifest already lists latest detected versions");
  process.exit(0);
}

manifest = `${manifest.trimEnd()}\n\n${additions.join("\n")}`;
fs.writeFileSync(manifestPath, `${manifest}\n`);

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
      source: "ci-latest-tools",
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
      source: "ci-latest-tools",
      store: "sqlite",
      fixtureRoot: "fixtures/native-import/opencode/1.17.13",
      notes:
        "CI latest-tool probe passed dry-run and isolated target-store writes against the current SQLite import layout.",
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

function hasExactToolVersion(manifest, tool, version) {
  const blocks = manifest.split("[[tools]]").slice(1);
  return blocks.some((block) => {
    return (
      tomlValue(block, "tool") === tool &&
      tomlValue(block, "version") === version &&
      tomlValue(block, "status") === "target-supported"
    );
  });
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
status = "target-supported"
source = "${entry.source}"
store = "${entry.store}"
fixture_root = "${entry.fixtureRoot}"
notes = "${entry.notes}"
`;
}
