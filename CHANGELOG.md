# Changelog

## 0.1.0 - 2026-11-23

The first release of claude-scope, a fork of [claude-vault](https://github.com/kuroko1t/claude-vault) 0.1.4 that adds a cscope-style menu for searching your Claude Code conversations.

### Added

- **A cscope-style menu**, opened by running `claude-scope` with no command. As in cscope, it's a column of search fields at the bottom of the screen:
  - **Search all conversations** searches every project.
  - **Search in a project** and **Search in a session** first ask which project or session, then search it as often as you like.
  - **Project sessions** lists projects, then a project's sessions, then a session's conversations.

  Matches are listed newest first, with three lines from where each match is. Enter opens the question and everything Claude answered in `$EDITOR`. The usual cscope keys work: Ctrl-N/Ctrl-P, Space, `+`, `-` and Ctrl-D.
- **`update`** archives new conversations, then opens the menu. It works like running `cscope` without `-d`.
- **`help`** lists the commands, and `help <command>` describes one.
- **Questions and answers.** Each conversation is split into questions and answers. Subagents' prompts are their own questions, and a slash command's prompt starts with the command you typed. Messages that Claude Code adds, such as summaries and interrupt markers, don't count as questions.

### Changed

- **Renamed from claude-vault.** The command, the database (`~/.local/share/claude-scope/vault.db`) and the variable `CLAUDE_SCOPE_DB` all use the new name. An existing claude-vault archive and `CLAUDE_VAULT_DB` are still used if there's no claude-scope one.
- **`--until` includes the whole date or time it names.** `--until 2024-06` covers all of June.
- **`--role` accepts only `user` or `assistant`.**

### Upgrading from claude-vault

claude-scope keeps using your claude-vault archive and upgrades it the first time it runs. Then run `claude-scope import` once, so that sessions whose transcripts are still in `~/.claude/projects/` have their subagents and slash commands sorted into questions like new ones.
