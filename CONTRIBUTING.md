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

## Checks

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):
`feat:` and `fix:` on `main` publish a release.
