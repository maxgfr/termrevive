# Code audit — 2026-11-10

Audit of termrevive 1.3.0 (`e53f729`), every source file read. Measurements were
taken on an Apple-silicon Mac, macOS 27, Rust 1.99, Ghostty 1.4.0 with 14 tabs
each running an agent session.

## Summary

- The codebase is small (about 4 800 lines, 8 direct dependencies), consistently
  structured (one file per terminal, one per agent CLI) and its parsers and
  script generators were already tested; fmt, clippy and the 49 tests passed.
- No critical defect in the code; the one critical finding was around it: CI and
  the release could no longer run (retired runner image, fixed, see below). Nine
  bugs sat in edge paths: the restore lock, rule rendering and quoting, the agent's
  environment, non-ASCII directories, stale Claude Code registry entries.
- All nine are fixed, eight with a regression test (49 → 56 tests).
- A save was dominated by one thing, the Ghostty read script: a whole
  `termrevive save` went from about 1.0 s to about 0.28 s.
- What remains is mostly design: the agent's log, the absence of timeouts on
  child processes, and the auto-save-only mode after a reboot (see below).

## Baseline

| Check | Before | After |
| --- | --- | --- |
| `cargo fmt --check` | pass | pass |
| `cargo clippy --locked --all-targets -- -D warnings` | pass | pass |
| `cargo test --locked` | 49 passed | 56 passed |
| `cargo build --release --locked` | pass, 2 281 184 bytes | pass, 1 465 248 bytes |
| `cargo audit` (77 crates) | no advisory | no advisory |

Nothing failed before the audit.

## Bugs fixed

| # | File | Issue | Fix | Test | Commit |
| --- | --- | --- | --- | --- | --- |
| 1 | `src/store.rs` | The restore lock was a pid file. A restore interrupted by Ctrl-C, SIGTERM or a kill left it behind (destructors do not run on a signal); once the pid was reused, typically after a reboot, auto-save skipped every tick with "a restore is running". Two restores could also run at once, and a pid of 0 or below counted as alive. | Advisory `flock` on the same file, released by the kernel when the process ends. A second restore is refused with "another restore is running". | `restore_lock_is_exclusive_and_ends_with_its_holder`; also reproduced with the 1.3.0 binary: a leftover file naming a live pid made `save --auto` print "skip: a restore is running". | `d470d3c` |
| 2 | `src/rules.rs` | `{session_cwd}` and `{claude_cwd}` went into a rule's `command` unquoted: `cd {session_cwd} && …` broke on a path with a space and let shell metacharacters through. `{cwd}` was shell-quoted even in a rule's `cwd` field, where the quotes made the path unusable. | Values are quoted when the template is a command and left as they are when it is a path. | `paths_are_quoted_in_commands_and_raw_in_cwd` | `e3488bc` |
| 3 | `src/rules.rs` | `shell_quote` left `=ls` unquoted (zsh expands it to `/bin/ls`) and `dir=~` (bash expands it). | A leading `=` and any `~` are quoted. | `quotes_what_a_shell_would_expand` | `6e4ff56` |
| 4 | `src/main.rs` | Only `restore --auto` recorded the boot as restored. If the restore at login failed before starting (a syntax error in `config.toml` is enough) and the windows were restored by hand, auto-save kept skipping until the next reboot, and restarting the agent restored a second time on top of the open windows. | A manual restore of every terminal in the snapshot records the boot too. One narrowed with `--backend` does not. | `only_a_restore_of_every_terminal_covers_the_snapshot` | `9b4bed9` |
| 5 | `src/launchd.rs` | launchd starts the agent without the shell's environment. With `XDG_STATE_HOME` set, the agent saved under `~/.local/state` while `list`, `show` and `restore` looked elsewhere, and the restore lock was not shared. Same for `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `VIBE_HOME`, `KITTY_CONFIG_DIRECTORY`. | `install` writes the ones that are set into the plist's `EnvironmentVariables`. | `plists_are_valid`, `plist_lists_every_argument` | `978419d` |
| 6 | `src/paths.rs` (new), `src/store.rs`, `src/rules.rs`, `src/backend/conf.rs`, `src/backend/kitty.rs` | `XDG_STATE_HOME=` (set to nothing) made the state directory the relative path `termrevive`, in the current directory; same for the config. | One helper for every lookup; an empty value counts as unset, as the XDG specification asks. | `an_empty_variable_counts_as_unset` | `7bfe7e2` |
| 7 | `src/procs.rs` | Working directories came from `lsof`, which escapes every byte its locale does not call printable. The agent runs without a locale: a pane in `~/Réunion` was saved as `~/R\xc3\xa9union`, and restored in the home directory. Confirmed with `lsof` run without `LANG`/`LC_*`. | The path is read from the kernel (`proc_pidinfo`, `PROC_PIDVNODEPATHINFO`). | `reads_a_working_directory_with_spaces_and_accents` | `cf5db6b` |
| 8 | `src/harness/claude.rs` | Any process whose pid had a `~/.claude/sessions/<pid>.json` was taken for Claude Code. A CLI that is killed leaves its entry; a process that later gets the pid was saved as `claude --resume <old session>`, and the pane really running that session lost it. | An entry written more than 60 s before the process started is ignored (`startedAt`; on the 15 live entries checked it is 0.5 to 143 s after the process start). The test no longer changes `CLAUDE_CONFIG_DIR`, which raced with tests reading the environment in other threads. | `an_entry_older_than_the_process_is_a_reused_pid` | `c526834` |
| 9 | `src/store.rs` | A manual save and the agent's both wrote `latest.json.tmp` before renaming it; overlapping, they could leave an unreadable `latest.json`. | The temporary file is named after the process. | None: the race cannot be reproduced deterministically. | `4c5648a` |

Bug 6 was found while fixing bug 5 and was not in the initial plan.

## Optimizations

| What | Before | After | Commit |
| --- | --- | --- | --- |
| Ghostty read script: each property is asked for all tabs, or all terminals of all tabs, in one Apple event (five per window) instead of one event per property per pane. The old script stays as a fallback. | `termrevive save`, 14 tabs: about 1.0 s (4.7 to 5.5 s for 5 saves) | about 0.28 s (1.25 to 1.6 s for 5 saves), identical snapshot | `23517f6` |
| Working directories from the kernel instead of two `lsof` processes per terminal per save (bug 7). | 18 to 20 ms per call, 68 processes | 0.15 to 0.23 ms | `cf5db6b` |
| Active Codex threads listed once per capture instead of once per Codex pane, and without a `stat` per file. | 38 ms per walk, per pane (360 days, 3 600 rollouts, synthetic) | 9 ms, once per capture | `c1a126e` |
| `restore` reuses the process list it already has when counting a terminal's open panes; auto-save reads the agent's plist only after a reboot. | one extra `ps` (84 ms) per running terminal; one `plutil` (about 3 ms) per tick | none | `f4b4fc1` |
| Release profile: `lto = true`, `codegen-units = 1`, `strip = true`. | 2 281 184 bytes, clean build about 10 s | 1 465 248 bytes (-36 %), clean build about 39 s; signature still valid (`codesign -v`) | `62b64c2` |

Measured and not done, the gain being too small for the code it needs:

- Caching the table lookup of the opencode/Kilo databases: 5 ms per pane.
- Parsing the built-in rules once: `show` on 20 000 panes takes 0.45 s, about
  22 µs per pane all included.
- `chrono` without its default features: no change in `Cargo.lock`.

## Quality

- `1356891` — what the backends repeated (pids of an emulator, splitting a
  script's output into lines, closing windows by id, the
  `ApplePersistenceIgnoreState` setting) lives once; the script closing windows
  gained a test. No behavior change.
- `883fc62` — lockfile updated (patch and minor releases; `toml` 1.1.8 → 1.2.1).

## Behavior changes

1. A second `termrevive restore` started while one runs now fails with "another
   restore is running" instead of running alongside (bug 1).
2. `{session_cwd}` / `{claude_cwd}` in a rule's `command` are shell-quoted when
   they need it; `{cwd}`, `{session_id}` and `{session_name}` in a rule's `cwd`
   are no longer quoted (bug 2). Arguments starting with `=` or containing `~`
   are typed back quoted (bug 3).
3. A manual restore of the whole snapshot lets auto-save resume and stops the
   agent from restoring again during that boot (bug 4).
4. `termrevive install` copies the seven variables listed in bug 5 into the
   plist when they are set. Run it again after changing one.
5. The released binary is built with LTO and stripped: a panic backtrace no
   longer has symbol names.
6. Ghostty is read with a different AppleScript, checked on Ghostty 1.4.0 only;
   older versions fall back to the previous script if it fails.

All of it is released as v1.3.1.

## CI and release: the runner image was retired

Both workflows pinned `runs-on: macos-14` (`ci.yml`, job `rust`; `release.yml`,
both entries of the build matrix), and GitHub retired every macOS 14 image from
Actions on 2026-11-02. The run started by this audit's first push was cancelled
within two seconds, before any step, with "The configuration
'macos-14-arm64-us-default' is not supported"; a re-run ended the same way.
Nothing pushed since that date was checked or released.

Workflows were out of this audit's scope; the change was made once the owner
asked for it: `macos-14` → `macos-15` in the three places (`bf0bfb4`). The run
that followed passed CI, built both targets (x86_64 included, which could not
be built locally) and published v1.3.1.

## Found, not fixed

### Critical

None.

### Important

1. **Auto-save without auto-restore can overwrite the snapshot you want back.**
   After a reboot, opening a terminal to type `termrevive restore` leaves at most
   two minutes before the agent saves that fresh window as `latest.json`. The
   previous snapshot survives in `snapshots/` (30 kept) and `--snapshot` brings
   it back, but nothing says so. Recommendation: when the latest snapshot is
   from this boot and none was restored, have `restore` offer the last snapshot
   of the previous boot, or keep a `latest-previous-boot.json`.
2. **iTerm2 and Terminal.app are read one Apple event per property per pane**,
   as Ghostty was. The same batching should apply (`name of sessions of tabs of
   w`…); it was not done because neither could be checked here (iTerm2 is not
   installed, and Terminal.app would have had to be driven for real).
3. **The agent's log grows without bound and repeats itself.** `agent.log` is
   227 KB after 16 days here; an unreadable terminal logs the same two lines
   every tick. Recommendation: log a skip or warning only when it changes, and
   cap the file (rename to `agent.log.1` past 1 MB at startup).
4. **No timeout on child processes** (`osascript`, `kitten`, `sqlite3`,
   `defaults`, `plutil`). One that hangs blocks the agent's loop for good, with
   nothing in the log. Recommendation: a wait with a deadline and a kill in the
   two helpers that spawn them (`applescript::run`, `kitty::remote`).
5. **`save` and `restore` in `main.rs` are not tested.** Their decisions (skip,
   pending restore, carried-over windows, marking the boot) are interleaved with
   I/O. Recommendation: pass the state directory, the boot time and the process
   table in, as bug 1's fix does for the lock, then test the decisions.
6. **A boot is identified by `kern.boottime` in seconds.** It was stable over the
   8 days of this boot, sleep included. It is derived from the wall clock
   though, so a clock step may shift it (not observed here); a change
   would make auto-save wait for a restore that already happened.
   `kern.bootsessionuuid` identifies a boot exactly. It needs a snapshot field,
   hence a decision.

### Minor

- `doctor` does not follow Ghostty's `config-file` or kitty's `include`: a
  setting made in an included file reads as unset.
- Snapshots and the log are created 0644 and hold command lines, which can carry
  secrets. The home directory is 0750 on current macOS, which hides them from
  other accounts; 0600 would not depend on that.
- A rule whose `command` has a literal `{…}` (`awk '{print $1}'`) or an unknown
  variable is skipped without a word. `doctor` could check the templates.
- kitty's `restore --dry-run` output is not shell-quoted (`--cwd=/a b`).
- kitty reconnects (one `ps`, one `lsof`, one `kitten @ ls`) for every window
  restored and again to close the blank one.
- `doctor --fix` sets kitty's `listen_on unix:/tmp/kitty`; `unix:${TMPDIR}/kitty`
  would put the socket in a per-user directory.
- SIGPIPE is reset to its default for the whole process, agent included: a
  child that exits before reading its input would end the agent without a log
  line (launchd restarts it).
- Claude Code is recognized by name alone: `claude -p …` and `claude mcp serve`
  on a pane's tty count as the interactive CLI, unlike the other harnesses,
  which exclude their subcommands.
- `dirs` is only used for the home directory, which `std::env::home_dir` gives
  (three crates fewer).
- `ps` (84 ms here) could be replaced by libproc, as `lsof` was. `login` being
  owned by root makes that less direct; not worth it at one call per save.
- Snapshot files are named from local time: around the autumn clock change, two
  can sort out of order for an hour (`latest.json` is not affected).

## Open questions

1. **The runner image**: now pinned to `macos-15`, which will be retired in its
   turn. Keep pinning, or use `macos-latest`?
2. **A user rule matching an agent with no `cwd`** runs in the pane's shell
   directory, not the session's. `claude --resume` needs the session's project
   directory, so a rule that only changes the command breaks sessions living in
   a worktree unless it also sets `cwd = "{session_cwd}"`. Should the session's
   directory be the default for agent sessions?
3. **`ssh` and `mosh` are re-run automatically** with their saved arguments,
   including a remote command (`ssh host 'rm -rf build'`) or port forwards.
   Should a saved `ssh` with a command be typed without Enter?
4. **The pre-reboot snapshot in auto-save-only mode** (Important 1): protect it,
   or document the two-minute window?
5. **Log policy** (Important 3): only changes, a size cap, or both?
6. **Snapshot permissions**: 0600?
7. **Boot identity** (Important 6): add `kern.bootsessionuuid` to snapshots?
8. **The release profile**: 0.8 MB saved for about 30 s more per release build
   and per `cargo install`. Keep it, or only `strip` (-21 %, no build cost)?
