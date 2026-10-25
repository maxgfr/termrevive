# Contributing

## Adding a terminal

A terminal is one module in `src/backend/` implementing the `Backend` trait
(`src/backend/mod.rs`). Everything else — finding shells and foreground
programs, Claude Code sessions, pairing panes with shells, rules, snapshots,
LaunchAgents, `doctor` — is shared and needs no change.

1. **Pick the closest existing backend and copy it.**
   - Scriptable through AppleScript: `iterm2.rs` (tabs and splits, reports
     ttys), `terminal.rs` (no splits, reports ttys) or `ghostty.rs` (no tty:
     pairing by score).
   - Driven by a CLI that prints JSON: `kitty.rs` (`kitten @ ls`, reports shell
     pids).
2. **Implement the trait.**
   - `read_layout` returns windows → tabs → panes. Fill `RawTerminal::tty`
     whenever the terminal can tell it (directly, or from a shell pid through
     the process table it receives): those panes are paired exactly. Leave it
     `None` and `host_pids` lets capture find the shells among the emulator's
     children, paired by title and cwd.
   - `restore_window` rebuilds one window from a `WindowPlan`: each pane's
     `Launch` has a `cwd`, an optional `input` and `run` (press Enter or only
     type it). If the terminal cannot open a pane in a directory, type
     `cd_line(cwd)` first. `restore_script` returns the same thing as text for
     `--dry-run`.
   - `caps().splits = false` makes restores turn extra panes into tabs.
   - `settings` / `apply_setting` cover what must hold for a restore (usually:
     the terminal must not reopen its own windows at launch). `doctor` shows
     them and `doctor --fix` applies them.
   - Shared helpers: `applescript.rs` (`run`, `str` escaping, `delay`,
     `launch`, `wait_ready`, `records`), `conf.rs` (`key value` / `key = value`
     config files), `defaults.rs` (`defaults read/write`).
3. **Register it**: one line in `registry()` in `src/backend/mod.rs`.
4. **Test it** with fixtures, no terminal needed: parse a captured read output
   (AppleScript records or JSON) into a `RawLayout`, and check the restore
   script or commands generated for a `WindowPlan` with tabs, a split, a
   selected tab, `run = true` and `run = false`. See the tests at the bottom of
   each backend.
5. **Document it**: a row in the README support table.
6. **Check it for real** before opening the pull request:
   - [ ] `termrevive --backend <name> save` then `show`: every pane with its
         program and directory, the selected tab marked.
   - [ ] Close the windows, `termrevive --backend <name> restore`, `save` again:
         same layout.
   - [ ] A Claude Code session comes back with `claude --resume`, and
         `~/.claude/sessions/*.json` shows the same `sessionId`.
   - [ ] With the terminal closed, `restore` starts it and closes its blank
         startup window.
   - [ ] `termrevive doctor` and `doctor --fix`.

## Adding a coding agent (harness)

A harness is one module in `src/harness/` implementing the `Harness` trait
(`src/harness/mod.rs`): it recognizes its CLI among a pane's processes, finds
the session that process works on, and says how to resume it. Capture, pairing,
rules (`match = "<name>"`, `{session_id}`), snapshots and restore are shared.

1. **Find how a running CLI maps to its session.** Start a throwaway session
   and look at what the agent writes while it runs:
   - a registry keyed by pid (`~/.claude/sessions/<pid>.json`, Vibe's
     `active/*.lock.json`) is exact — see `claude.rs`, `vibe.rs`;
   - otherwise, sessions that are active or recently updated, matched by the
     process' directory and start time — see `codex.rs` (files) and
     `sqlite.rs` (an SQLite database, read with macOS' `sqlite3`).
   `AgentProc` gives you the pid, tty, argv, cwd and start time; the whole
   process table is there too, for helpers that run beside the CLI.
2. **Implement the trait.**
   - `is_cli`: the interactive CLI only — not its daemon, MCP server or one-shot
     subcommands.
   - `sessions`: candidates, likeliest first. Several CLIs of one harness in a
     pane each get a different session: capture hands out each id once.
   - `resume_command`: what to type in the pane's shell (quote ids with
     `rules::shell_quote`).
   - Honor the agent's own home override (`CODEX_HOME`, `VIBE_HOME`…) through
     `home_dir_from` / `xdg_data`.
3. **Register it**: one line in `registry()` in `src/harness/mod.rs`.
4. **Test it** with fixtures in a temp dir (session files, a small SQLite
   database): which session a process gets, that stale or foreign sessions are
   ignored, the resume command, and that non-interactive processes are not
   taken for the CLI.
5. **Document it**: a row in the README "Supported coding agents" table.
6. **Check it for real**:
   - [ ] Two panes running the agent (send a prompt in each), `termrevive save`
         then `show`: each pane shows `run <resume command>` with its own id.
   - [ ] Close them, `termrevive restore`: each agent reopens its conversation.

## Checks

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):
`feat:` and `fix:` on `main` publish a release.
