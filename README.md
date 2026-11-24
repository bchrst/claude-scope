# claude-scope

[![CI](https://github.com/bchrst/claude-scope/actions/workflows/ci.yml/badge.svg)](https://github.com/bchrst/claude-scope/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/claude-scope.svg)](https://crates.io/crates/claude-scope)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**cscope for your Claude Code conversations.**

If you know cscope, you already know claude-scope. It archives every conversation you've had with Claude Code into a full-text index, and lets you search it the cscope way: a column of search fields at the bottom of the screen, numbered matches, and Enter to read one in your `$EDITOR`. Quit the editor and you're back at the list.

```
claude-scope · version 0.1.0

  Type what to look for in one of the fields below and press Enter.

  Search in a project or a session: choose which first, then type what to look for
  in it, as often as you like. Anything typed in the field already is searched for
  as soon as you choose.

  Project sessions: choose a project, then one of its sessions to browse. With
  something typed, only the projects with matches are listed, then their matches.

Search all conversations: docker
Search in a project:      (choose a project, then search it)
Search in a session:      (choose a session in ~/src/webapp, then search it)
Project sessions:         (choose a project, then one of its sessions)
↑/↓ choose the search · Enter search · Esc clear
```

Enter lists the matches, newest first. Under each question are three lines from where the match is, marked `Q:` if they're from your question and `A:` if they're from Claude's answer:

```
"docker" · 4 matches · all projects

> 1  2026-03-13 15:45  ~/src/webapp  Add a healthcheck to the compose file
     A: Added a healthcheck to docker-compose.yml that polls /healthz every 10s.
        The web service now waits for the database to be healthy before starting.

  2  2026-03-13 15:03  ~/src/webapp  Can the image be smaller?
     A: The base image is the biggest part.
        Switching to the slim docker image saves 1.2 GB:
        FROM python:3.12-slim
  3  2026-03-13 14:21  ~/src/webapp  How do I fix the Docker build for the GPU cont…
     Q: How do I fix the Docker build for the GPU container?


  4  2026-03-12 23:41  ~/src/infra   The docker registry is out of disk
     Q: The docker registry is out of disk

↑/↓ or 1-4, Enter open · Esc back · q quit
```

Type `2` and press Enter, and the question opens in your editor as Markdown, with everything Claude replied before your next question:

```markdown
# ~/src/webapp · 2026-03-13 15:03 · session 9264b6e0-f42a-4a88-9fe8-4b2e9484afef

## Question

Can the image be smaller?

## Answer

The base image is the biggest part.
Switching to the slim docker image saves 1.2 GB:
FROM python:3.12-slim
and a multi-stage build keeps the compilers out of the final image.
```

It's a single binary with no runtime dependencies: everything is kept in one SQLite file.

## Why archive?

Claude Code keeps each session as a JSONL file under `~/.claude/projects/`, but:

- **Old sessions are deleted.** Claude Code removes them after a while (the `cleanupPeriodDays` setting). Even if you change this, the other problems remain.
- **`/compact` loses detail.** It summarizes the conversation, and the details of what was said are lost.
- **You can't search them.** The files are spread over a directory per project, and nothing searches across sessions.

claude-scope copies the conversations into a SQLite database with full-text search. Once archived, they survive deletion, compaction and cleanup.

## Coming from cscope

### Commands

| cscope | claude-scope |
|--------|--------------|
| `cscope.out`, the cross-reference | `~/.local/share/claude-scope/vault.db`, a SQLite full-text index of `~/.claude/projects/` |
| `cscope -b`: build the cross-reference without opening the menu | `claude-scope import` |
| `cscope`: update the cross-reference, then open the menu | `claude-scope update` |
| `cscope -d`: open the menu without updating | `claude-scope` |
| `cscope -f reffile`: use another cross-reference | `claude-scope --db file`, or `CLAUDE_SCOPE_DB=file` |
| `cscope -L -4 text`: one search, printed line by line | `claude-scope search text`, and `--json` for scripts |
| Run it at the top of the source tree | Run it in the project's directory: "Search in a project" preselects that project, and "Search in a session" lists its sessions |

### Keys

| cscope | claude-scope |
|--------|--------------|
| ↑/↓ or Ctrl-N/Ctrl-P: next or previous field | ↑/↓, Ctrl-N/Ctrl-P, or Tab/Shift-Tab |
| Enter: search | Enter |
| `0`–`9`, `a`–`z`: edit that match | Type the entry's number, or select it with ↑/↓ (or `j`/`k`), then press Enter |
| Space or `+`: next page; `-`: previous page | Space, `+` or PgDn; `-`, `b` or PgUp |
| `EDITOR` | `VISUAL`, or else `EDITOR` (`vi` if neither is set) |
| Ctrl-D: exit | Ctrl-D or Ctrl-C, or `q` in a list |
| — | Esc: back a level, from a list to where you chose it, and from the menu out |

### What's different

- **The matches have the screen to themselves.** Instead of using Tab to go between the fields and the matches, Enter goes to the matches and Esc goes back to the menu. What you typed is still in the field, ready to change and search again.
- **Opening a match takes Enter.** Entries are numbered through the whole list rather than per page, so a number can have several digits. Typing them selects the entry, and Enter opens it.
- **You open a conversation, not a line of a file.** The editor shows a temporary Markdown copy of one question and its answer, and anything you change is thrown away.
- **Search is full-text search, not grep.** All the words you type must appear, and they match whole words, with stemming ("running" matches "run"). Use quotes for a phrase, and `AND`, `OR` and `NOT` ([FTS5 syntax](https://www.sqlite.org/fts5.html#full_text_query_syntax)). There are no regular expressions. A search lists its 200 best matches (`--limit`), newest first.
- **There's no Ctrl-R to rebuild.** To archive new conversations, quit and run `claude-scope update`, or let [hooks](#auto-archive-with-claude-code-hooks) archive them as you work.
- **Searches can be narrowed by where they look.** cscope's fields choose what kind of thing to find in the code. Here the fields choose where to look: everywhere, in one project, or in one session.

## The menu

- **Search all conversations** searches every conversation in every project.
- **Search in a project** first asks which project to search. All projects are listed, with the current directory's selected. Then type what to look for in it and press Enter. Going back from the matches returns to that prompt, so you can search the same project again. Anything already typed in the menu's field is searched for as soon as you choose the project.
- **Search in a session** works the same way for one session, chosen from the current directory's project. From a subdirectory, it uses the closest parent directory with sessions (but not your home directory). Outside any project, or with `--project`, it lists the sessions of all the projects searched.
- **Project sessions** is for browsing. It lists all projects; choose one to list its sessions, then a session to list its conversations. With something typed, it lists only the projects with matches, with how many each has, and then the best matches in the project you choose.

All lists are newest first. A session's conversations show when each question was asked, and questions Claude Code gave to subagents are marked `(subagent)`:

```
2026-03-13 14:21 · 12 questions · session 47cf1f2e

> 1  15:40  (subagent) Find what makes the image large
  2  15:02  Can the image be smaller?
  3  14:21  How do I fix the Docker build for the GPU container?
```

Some more about using it:

- **Editors.** GUI editors must wait for the file to be closed, for example `EDITOR="code --wait"`.
- **Tool calls.** They're hidden in answers unless you pass `--include-tools`.
- **Options.** `--project`, `--since` and `--until` narrow what's listed and searched, `--role` matches only your messages or only Claude's, and `--limit` sets how many matches a search lists (0 for all).
- **Starting with a search.** Run `claude-scope i "docker"` to search all conversations as soon as it starts.

## Install

### From GitHub Releases (recommended)

Download a prebuilt binary from [Releases](https://github.com/bchrst/claude-scope/releases). There are builds for Linux and macOS, on x86_64 and ARM:

```bash
# Linux x86_64 (or aarch64-unknown-linux-gnu)
curl -fsSL https://github.com/bchrst/claude-scope/releases/latest/download/claude-scope-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo mv claude-scope /usr/local/bin/

# macOS Apple Silicon (or x86_64-apple-darwin)
curl -fsSL https://github.com/bchrst/claude-scope/releases/latest/download/claude-scope-aarch64-apple-darwin.tar.gz | tar xz
sudo mv claude-scope /usr/local/bin/
```

### From crates.io

```bash
cargo install claude-scope
```

### From source

```bash
cargo install --path .
```

Building needs Rust 1.85 or later.

## Quick start

1. Archive the conversations you have now. This is `cscope -b`:

   ```bash
   claude-scope import
   # Imported 94562 messages (0 skipped, 12847 filtered, 0 errors) from 203 files
   ```

2. Open the menu from a project's directory:

   ```bash
   cd ~/src/my-project
   claude-scope
   ```

   Run `claude-scope update` instead to archive any new conversations first.

3. Optionally, [set up hooks](#auto-archive-with-claude-code-hooks) so that conversations are archived as you work.

## Scripting, and searching from Claude Code

Like `cscope -L`, the other commands print their results and exit, so you can use them in scripts and pipelines, or let Claude search your past conversations:

```bash
claude-scope search "docker"                       # print the matching messages
claude-scope search '"docker image" OR registry'   # FTS5 syntax
claude-scope search "auth bug" --json              # structured output for scripts or Claude
claude-scope list --json
claude-scope export --last --format markdown > session.md
```

`search` prints each matching message:

```
$ claude-scope search docker --project infra
[user] src/infra | 3ee3d806-adcd-4a4c-9a7d-19d9fd3192a6 | 2026-03-12T23:41:00.000Z
The docker registry is out of disk
```

`list` lists sessions, newest first:

```
$ claude-scope list -n 3
ID         DATE                    MSGS  PROJECT                   PREVIEW
----------------------------------------------------------------------------------------------------
9264b6e0   2026-03-13T14:21:00        6  src/webapp                How do I fix the Docker build for the GPU containe...
3ee3d806   2026-03-12T22:59:00        4  src/infra                 Move the staging cluster to the new VPC
cd6fbe88   2026-03-11T09:05:00        4  src/webapp                Why does the login form submit twice?

Export: claude-scope export <ID>
```

With [hooks](#auto-archive-with-claude-code-hooks) set up, Claude Code can search all your history this way, even sessions whose JSONL files have been deleted.

<details>
<summary><h2>Command reference</h2></summary>

Run `claude-scope help` for all commands, and `claude-scope help <command>` for one.

### interactive

The menu. This is what `claude-scope` does when run without a command. Until something has been imported, it asks you to run `claude-scope import` first.

```bash
claude-scope                                        # same as: claude-scope interactive, or claude-scope i
claude-scope i "docker"                             # start by searching all conversations
claude-scope i --project my-app                     # only projects whose name contains my-app
claude-scope i --since 2026-03-01 --role user       # match only your own messages, from March on
```

`--since` and `--until` limit the sessions listed by when they started, and the matches by when they were written. `--limit` keeps the most relevant matches of a search (200 by default; 0 for all).

### update

Import new conversations from `~/.claude/projects/` (as `import` does), then open the menu, which says how many new messages were archived. It takes the same options as `interactive`, plus `--claude-dir`:

```bash
claude-scope update                                 # refresh the archive, then browse it
claude-scope update "docker" --project my-app       # refresh, then search
```

Without a terminal it imports nothing and fails; use `import` in scripts and hooks.

### import

Import all JSONL files from `~/.claude/projects/` recursively, including subagent directories. Messages already archived are skipped, so it's safe to run often:

```bash
claude-scope import
```

### import-file

Import a single JSONL session file:

```bash
claude-scope import-file /path/to/session.jsonl --project my-project
```

### search

```bash
claude-scope search "Docker"
claude-scope search "deploy" --project my-app
claude-scope search "deploy" --since 2024-01-01 --until 2024-06   # through the end of June
claude-scope search '"error handling" AND rust'   # FTS5 syntax
claude-scope search "auth bug" --json              # machine-readable output
```

### list

```bash
claude-scope list
claude-scope list --project my-app --since 2024-01-01
claude-scope list --json
```

### export

Export a session to Markdown, JSON, or plain text. Session IDs can be shortened, like git commit hashes:

```bash
claude-scope export 47cf1f2e
claude-scope export --last                          # most recent session
claude-scope export --last --format markdown > session.md
```

### Other commands

```bash
claude-scope delete 47cf -y                         # delete a session
claude-scope stats                                  # show database statistics
claude-scope verify                                 # check database integrity
claude-scope completions zsh > ~/.zfunc/_claude-scope  # shell completions
claude-scope --db /path/to/vault.db search "query"  # use another database
```

</details>

## Auto-archive with Claude Code Hooks

Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreCompact": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "claude-scope import >/dev/null 2>&1"
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "claude-scope import >/dev/null 2>&1 &"
          }
        ]
      }
    ]
  }
}
```

| Hook | Timing | Mode |
|------|--------|------|
| **PreCompact** | Before `/compact` runs | Synchronous — captures full data before compression |
| **SessionEnd** | When a session ends | Background — non-blocking |

Once configured, conversations are archived automatically with no manual steps.

<details>
<summary><h2>How It Works</h2></summary>

```
~/.claude/projects/<project>/<session>.jsonl
        │
        ▼
    ┌─────────┐     ┌──────────┐     ┌───────────┐
    │  Parse   │────▶│  Filter  │────▶│  SQLite   │
    │  JSONL   │     │  & Clean │     │  + FTS5   │
    └─────────┘     └──────────┘     └───────────┘
```

1. **Parse** — Reads each JSONL record, extracts `user` and `assistant` messages
2. **Filter** — Removes system-injected noise (see below)
3. **Store** — Inserts into SQLite with UUID-based dedup; FTS5 index is updated via triggers

### Noise Filtering

| Category | What's removed |
|----------|---------------|
| Tool results | All `tool_result` content (Bash output, file creation messages, web fetch results, etc.) |
| System tags | `<system-reminder>`, `<local-command-caveat>`, `<local-command-stdout>`, `<command-name>`, `<command-message>`, `<command-args>` |
| Read-only tools | Read, Glob, Grep, LSP, ToolSearch, browser snapshot/navigation, TaskGet/TaskOutput/TaskList |
| Meta messages | eval-loop iterations/commands, Stop hook feedback, empty/whitespace-only content |

User text input, assistant responses, and code-modifying tool calls (Edit, Write, Bash, etc.) are preserved.

</details>

<details>
<summary><h2>Schema</h2></summary>

```sql
CREATE TABLE sessions (
    session_id  TEXT PRIMARY KEY,
    project     TEXT NOT NULL,
    started_at  TEXT,
    imported_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(session_id),
    uuid       TEXT,
    role       TEXT NOT NULL,
    content    TEXT NOT NULL,
    timestamp  TEXT,
    agent_id   TEXT,                       -- subagent that wrote it; NULL = main conversation
    is_meta    INTEGER NOT NULL DEFAULT 0  -- added by Claude Code, not typed by the user
);

CREATE UNIQUE INDEX idx_messages_uuid ON messages(uuid) WHERE uuid IS NOT NULL;

CREATE VIRTUAL TABLE messages_fts USING fts5(
    content, content_rowid='id', content='messages',
    tokenize='porter unicode61'
);
```

Default database location: `~/.local/share/claude-scope/vault.db`. If that doesn't exist yet but a claude-vault archive does (`~/.local/share/claude-vault/vault.db`), that one is used. Set `CLAUDE_SCOPE_DB` or `--db` to use another file (`CLAUDE_VAULT_DB` is still read when neither is set).

SQLite is configured with WAL mode and a 5-second busy timeout for safe concurrent access.

</details>

## Credits

claude-scope is a fork of [claude-vault](https://github.com/kuroko1t/claude-vault) by kuroko1t, which archives and searches the conversations. claude-scope adds the cscope-style menu.

## License

MIT
