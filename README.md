# termrevive

Save your terminal sessions and get them back after a reboot: windows, tabs,
splits, working directories **and the programs that were running** — every
Claude Code session resumes its own conversation (`claude --resume <id>`, in the
worktree it was in), editors and `ssh` reopen, unknown commands are typed back
without being run.

Backends: **Ghostty** (macOS, ≥ 1.3, via its AppleScript dictionary). The
backend trait (`src/backend/mod.rs`) is the place to add iTerm2, kitty, WezTerm…

## Install

```sh
brew install maxgfr/tap/termrevive
# or from source
cargo install --git https://github.com/maxgfr/termrevive

termrevive doctor
```

Add `window-save-state = never` to `~/.config/ghostty/config`: otherwise Ghostty
reopens its own empty windows at launch on top of the restored ones.

The first run asks macOS for *Automation → Ghostty* permission.

## Use

```sh
termrevive save              # snapshot now
termrevive show              # what the latest snapshot holds and what restore would run
termrevive restore           # after a reboot, from a fresh Ghostty window
termrevive restore --dry-run # print the AppleScript instead
termrevive list              # snapshots, newest first (last 30 kept)
termrevive restore --snapshot ~/.local/state/termrevive/snapshots/<file>.json
```

`restore` refuses to run when Ghostty already has more than one pane open (it
would duplicate them); `--force` overrides that.

### Automatic (optional)

```sh
termrevive install                 # both agents
termrevive install --auto-save     # save every 2 min (--interval to change)
termrevive install --auto-restore  # restore once at login
termrevive uninstall
```

- Auto-save writes only when something changed, never while a restore runs,
  never when Ghostty is closed or empty, and — when auto-restore is installed —
  not after a reboot until that boot's restore has happened, so a fresh empty
  Ghostty can't overwrite the snapshot you want back.
- Auto-restore runs at login, only from a snapshot of a previous boot, once per
  boot. It starts Ghostty if needed and closes its blank startup window.
- Logs: `~/.local/state/termrevive/logs/`.

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

Ghostty's scripting API lists windows → tabs → terminals with their title and
working directory, but no tty or PID. termrevive reads the process table
(`ghostty → login → shell → foreground job`, exact argv via `KERN_PROCARGS2`,
cwd via `lsof`) and Claude Code's `~/.claude/sessions/*.json`, then pairs
terminals with shells by score: Claude session name in the title, program name
in the title, same cwd. Anything it can't place is still saved, in an extra
"orphans" window.

## Limits

- Split geometry isn't exposed by Ghostty: panes of a tab come back as
  side-by-side splits, in order.
- A program's in-memory state is not saved (unless the program itself resumes,
  like Claude Code).
- macOS only for now.
