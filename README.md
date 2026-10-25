# termrevive

Save your terminal sessions and get them back after a reboot: windows, tabs,
splits, working directories **and the programs that were running** — every
Claude Code session resumes its own conversation (`claude --resume <id>`, in the
worktree it was in), editors and `ssh` reopen, unknown commands are typed back
without being run.

## Supported terminals (macOS)

| Terminal | Tabs | Splits | Pane ↔ shell pairing | Needs | `doctor --fix` sets |
| --- | --- | --- | --- | --- | --- |
| **Ghostty** ≥ 1.3 | yes | yes (side by side) | by score (title, cwd) | Automation permission | `window-save-state = never` |
| **iTerm2** | yes | yes (side by side) | exact (tty) | Automation permission | `ApplePersistenceIgnoreState` |
| **kitty** | yes | yes (in the tab's layout) | exact (shell pid) | remote control on a unix socket | `allow_remote_control`, `listen_on` in kitty.conf |
| **Terminal.app** | each tab comes back as a window | — | exact (tty) | Automation permission | `ApplePersistenceIgnoreState` |

One snapshot holds every terminal that runs; `--backend <name>` (`ghostty`,
`iterm2`, `kitty`, `terminal`) restricts any command to one of them. Missing
yours? Adding a terminal is one file — see [CONTRIBUTING.md](CONTRIBUTING.md),
or open a *Terminal support request* issue.

## Install

```sh
brew install maxgfr/tap/termrevive
# or from source
cargo install --git https://github.com/maxgfr/termrevive

termrevive doctor
termrevive doctor --fix   # once: the settings in the table above
```

Terminals must not reopen their own windows at launch, on top of the restored
ones: `doctor --fix` turns that off for every installed terminal it supports
(`termrevive install --auto-restore` does it too). kitty has to be restarted
after its fix. The first run asks macOS for *Automation* permission per
terminal.

## Use

```sh
termrevive save              # snapshot now
termrevive show              # what the latest snapshot holds and what restore would run
termrevive restore           # after a reboot, from a fresh terminal window
termrevive restore --dry-run # print what would run instead
termrevive list              # snapshots, newest first (last 30 kept)
termrevive restore --snapshot ~/.local/state/termrevive/snapshots/<file>.json
```

`restore` refuses to run when a terminal already has more than one pane open
(it would duplicate them); `--force` overrides that, `--backend` restores one
terminal only.

### Automatic (optional)

```sh
termrevive install                 # both agents
termrevive install --auto-save     # save every 2 min (--interval to change)
termrevive install --auto-restore  # restore once at login
termrevive uninstall
```

- Auto-save writes only when something changed, never while a restore runs,
  never when no terminal runs, and — when auto-restore is installed — not after
  a reboot until that boot's restore has happened, so a fresh empty terminal
  can't overwrite the snapshot you want back. A terminal that runs but cannot
  be read keeps its windows from the previous snapshot.
- Auto-restore runs at login, only from a snapshot of a previous boot, once per
  boot. It starts each terminal if needed and closes its blank startup window.
  Installing it also applies `doctor --fix`.
- Logs: `~/.local/state/termrevive/logs/`.

## Agent skill

```sh
npx skills add maxgfr/termrevive            # pick the agents interactively
npx skills add maxgfr/termrevive -a claude-code
```

The skill teaches Claude Code, Codex and other agents to save before a restart,
check `show` before restoring, and ask before `--force`, `install` or
`doctor --fix`. `termrevive skill` prints the copy that ships with the installed
CLI.

## Rules

`termrevive config --init` writes a commented `~/.config/termrevive/config.toml`.
Rules map the foreground program to what gets typed in the new pane:

```toml
[restore]
delay_ms = 300            # pause between tabs, so 100 Claude sessions don't start at once

[[rule]]
match = ["npm", "pnpm"]
command = "{argv}"
run = false               # type it, don't press Enter

[default]                 # anything not matched
command = "{argv}"
run = false
```

Your rules come first, then the built-ins (`builtin_rules = false` to drop them):
`claude` → `claude --resume {claude_session_id}` in `{claude_cwd}`; editors,
pagers, `ssh`, `htop`, `lazygit`… → rerun `{argv}`. Variables: `{argv}`, `{cwd}`,
`{claude_session_id}`, `{claude_cwd}`, `{claude_name}`. Commands are typed in
your interactive shell, so aliases (`claude=…`) apply.

## How it works

Each terminal's scripting interface (AppleScript, or `kitten @` for kitty) lists
windows → tabs → panes. termrevive reads the process table (shell and
foreground job of each tty, exact argv via `KERN_PROCARGS2`, cwd via `lsof`) and
Claude Code's `~/.claude/sessions/*.json`, then pairs every pane with its shell:
exactly when the terminal reports the pane's tty (iTerm2, Terminal.app) or
shell pid (kitty), by score otherwise (Ghostty: Claude session name in the
title, program name in the title, same cwd). Anything it can't place is still
saved, in an extra "orphans" window.

## Limits

- Split geometry isn't exposed: panes of a tab come back as side-by-side splits
  (Ghostty, iTerm2) or in the tab's layout (kitty), in order.
- Terminal.app's AppleScript shows each tab as a window and cannot open tabs, so
  its tabs come back as separate windows.
- A program's in-memory state is not saved (unless the program itself resumes,
  like Claude Code).
- macOS only for now.
