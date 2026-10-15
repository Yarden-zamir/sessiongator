---
name: sessiongator
description: Find, read, resume, and convert past coding-agent sessions of Claude Code, opencode, Codex, and Copilot with the `sessiongator` CLI. Use when the user asks to find an earlier session or conversation, recall what was decided or done before, continue earlier work, or move a session to another tool.
license: MIT
---

# Sessiongator

`sessiongator` reads the session stores of Claude Code, opencode, Codex, and Copilot.
Use it instead of reading those stores by hand.

If `sessiongator` is missing, run `brew install yarden-zamir/tap/sessiongator`.

## Find a session

A session reference is an id, a unique id prefix, `tool:id`, or text that matches one title, path, or model.

- `sessiongator search '<text>'` searches transcripts, titles, paths, and models. Use it to find a topic.
- `sessiongator list '<text>'` matches only title, path, and model. Use it to list sessions of a project.
- Filters for both commands:
  - `--tool <tool>` limits to one tool. Repeat it for more tools.
  - `--here` limits to the current git project.
  - `--dir <dir>` limits to a folder and below.
  - `--sort <updated|created|msgs|path>` changes the order.
  - `-n <count>` limits the number of rows.
- Add `--json` when a script reads the output. Each line is one session.
- Output columns without `--json`: tool, id, age, folder, title, and for `search` a text snippet.

Examples:

```sh
sessiongator search 'connection pool' --tool claude -n 5
sessiongator search migration --here --sort msgs -n 10
sessiongator list --dir ~/Github/kitshn --tool opencode
```

If many sessions match, narrow with `--here`, `--tool`, or a more exact phrase before you open one.

## Read a session

```sh
sessiongator show <session>                  # full transcript
sessiongator show <session> --grep migration # only turns that contain the text
sessiongator show <session> --json
```

Read the transcript to answer "what did we decide" or "what was done".
Quote the session title and id when you report what you found.

## Resume a session

`sessiongator resume <session>` starts the owning tool and takes over the terminal.
In an agent shell, add `--print` to get the command, then give it to the user.

```sh
sessiongator resume 'rate limiter' --print
sessiongator resume <session> --here --print   # stay in the current folder
```

## Convert a session

Run the check first. Write only after the user agrees.

```sh
sessiongator compat                                  # installed tool versions and support level
sessiongator convert <session> --to codex --dry-run  # read, check, and map without writing
sessiongator convert <session> --to codex --mark-source
```

- `--to` takes `claude`, `opencode`, `codex`, or `copilot`.
- `--mark-source` adds a note to the source that names the new session.
- `--delete-source` moves the source to the trash.
- A target version that is not supported stops with exit code 3.
  Do not add `--allow-unsupported-version` unless the user asks for it.
- The command reads the written session back and checks it.
  On a mismatch it moves the new session to the trash and fails.

## Undo and delete

- `sessiongator history` lists conversions and deletes that `undo` can revert.
- `sessiongator undo` reverts the newest operation. Pass an operation id from `history` to revert an older one.
- `sessiongator undo --target` also moves the session that a conversion created to the trash.
- `sessiongator delete <session> --yes` moves a session to the sessiongator trash. It does not erase it.
  Run `delete` only when the user asks for it. Name the session title and id first.

## Do not

- Do not run `sessiongator` without a command. It opens an interactive picker.
- Do not edit session files in the tool stores by hand.
