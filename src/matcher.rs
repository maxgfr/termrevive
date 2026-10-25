//! Ties the terminals a backend reports to the shell sessions found in the
//! process table. A terminal that reports its panes' ttys is paired exactly;
//! for the others (Ghostty exposes no tty or PID) each pair is scored on title
//! and cwd and assigned greedily.

use crate::backend::{RawLayout, RawTerminal};
use crate::model::{Pane, Program, Tab, Window};

/// What we know about one shell session from the process side.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub tty: Option<String>,
    pub cwd: Option<String>,
    pub program: Option<Program>,
}

/// Drops the status glyph and spaces programs such as Claude Code put in front
/// of their title (`✳ my-session` → `my-session`).
pub fn strip_title(title: &str) -> &str {
    title.trim_start_matches(|c: char| !c.is_alphanumeric())
}

fn title_mentions(title: &str, name: &str) -> bool {
    let name = name.to_lowercase();
    title
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .any(|tok| tok == name)
}

fn score(t: &RawTerminal, c: &Candidate) -> u32 {
    let mut s = 0;
    if let Some(p) = &c.program {
        if p.claude
            .as_ref()
            .and_then(|c| c.name.as_deref())
            .is_some_and(|n| strip_title(&t.title) == n)
        {
            s += 100;
        }
        if title_mentions(&t.title, &p.name) {
            s += 20;
        }
    }
    if !t.cwd.is_empty() && c.cwd.as_deref() == Some(t.cwd.as_str()) {
        s += 10;
    }
    s
}

/// Builds the snapshot windows. Candidates left over once every terminal is
/// taken still get saved, in a trailing "orphans" window, if they run something.
pub fn assemble(layout: &RawLayout, candidates: Vec<Candidate>) -> Vec<Window> {
    let terminals: Vec<&RawTerminal> = layout
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .flat_map(|t| &t.terminals)
        .collect();

    let mut by_terminal: Vec<Option<usize>> = vec![None; terminals.len()];
    let mut used = vec![false; candidates.len()];
    // Exact: same tty.
    for (ti, t) in terminals.iter().enumerate() {
        let Some(tty) = &t.tty else { continue };
        if let Some(ci) =
            (0..candidates.len()).find(|&ci| !used[ci] && candidates[ci].tty.as_ref() == Some(tty))
        {
            by_terminal[ti] = Some(ci);
            used[ci] = true;
        }
    }

    let mut pairs: Vec<(u32, usize, usize)> = terminals
        .iter()
        .enumerate()
        .filter(|(ti, _)| by_terminal[*ti].is_none())
        .flat_map(|(ti, t)| {
            let used = &used;
            candidates
                .iter()
                .enumerate()
                .filter(move |(ci, _)| !used[*ci])
                .map(move |(ci, c)| (score(t, c), ti, ci))
        })
        .filter(|(s, _, _)| *s > 0)
        .collect();
    pairs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    for (_, ti, ci) in pairs {
        if by_terminal[ti].is_none() && !used[ci] {
            by_terminal[ti] = Some(ci);
            used[ci] = true;
        }
    }
    // Nothing in common at all: pair what is left in order rather than lose it.
    let mut free = (0..candidates.len()).filter(|&ci| !used[ci]);
    for slot in by_terminal.iter_mut().filter(|s| s.is_none()) {
        match free.next() {
            Some(ci) => *slot = Some(ci),
            None => break,
        }
    }
    drop(free);
    for ci in by_terminal.iter().flatten() {
        used[*ci] = true;
    }

    let mut next = 0;
    let mut windows: Vec<Window> = layout
        .windows
        .iter()
        .map(|w| Window {
            backend: String::new(),
            orphans: false,
            tabs: w
                .tabs
                .iter()
                .map(|t| Tab {
                    title: t.title.clone(),
                    selected: t.selected,
                    panes: t
                        .terminals
                        .iter()
                        .map(|term| {
                            let c = by_terminal[next].map(|ci| &candidates[ci]);
                            next += 1;
                            Pane {
                                title: term.title.clone(),
                                cwd: c
                                    .and_then(|c| c.cwd.clone())
                                    .unwrap_or_else(|| term.cwd.clone()),
                                program: c.and_then(|c| c.program.clone()),
                            }
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();

    let orphans: Vec<Tab> = candidates
        .iter()
        .zip(&used)
        .filter(|(c, used)| !**used && c.program.is_some())
        .map(|(c, _)| {
            let p = c.program.clone().unwrap();
            let title = p
                .claude
                .as_ref()
                .and_then(|c| c.name.clone())
                .unwrap_or_else(|| p.name.clone());
            Tab {
                title: title.clone(),
                selected: false,
                panes: vec![Pane {
                    title,
                    cwd: c.cwd.clone().unwrap_or_default(),
                    program: Some(p),
                }],
            }
        })
        .collect();
    if !orphans.is_empty() {
        windows.push(Window {
            backend: String::new(),
            tabs: orphans,
            orphans: true,
        });
    }
    windows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{RawTab, RawWindow};
    use crate::model::ClaudeSession;

    fn term(title: &str, cwd: &str) -> RawTerminal {
        RawTerminal {
            id: String::new(),
            title: title.into(),
            cwd: cwd.into(),
            tty: None,
        }
    }

    fn claude(name: Option<&str>, sid: &str, cwd: &str) -> Candidate {
        Candidate {
            tty: None,
            cwd: Some(cwd.into()),
            program: Some(Program {
                name: "claude".into(),
                argv: vec!["claude".into()],
                claude: Some(ClaudeSession {
                    session_id: sid.into(),
                    cwd: cwd.into(),
                    name: name.map(Into::into),
                }),
            }),
        }
    }

    fn layout(tabs: Vec<Vec<RawTerminal>>) -> RawLayout {
        RawLayout {
            windows: vec![RawWindow {
                tabs: tabs
                    .into_iter()
                    .enumerate()
                    .map(|(i, terminals)| RawTab {
                        title: String::new(),
                        selected: i == 0,
                        terminals,
                    })
                    .collect(),
            }],
        }
    }

    fn session_ids(w: &[Window]) -> Vec<String> {
        w.iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .map(|p| {
                p.program
                    .as_ref()
                    .and_then(|p| p.claude.as_ref())
                    .map(|c| c.session_id.clone())
                    .unwrap_or_else(|| "-".into())
            })
            .collect()
    }

    #[test]
    fn claude_titles_beat_order_and_unnamed_falls_out_by_elimination() {
        let l = layout(vec![
            vec![term("✳ beta", "/d")],
            vec![term("◐ Claude Code", "/d")],
            vec![term("◑ alpha", "/d/pdf")],
        ]);
        let c = vec![
            claude(Some("alpha"), "A", "/d/pdf"),
            claude(None, "X", "/d"),
            claude(Some("beta"), "B", "/d"),
        ];
        let w = assemble(&l, c);
        assert_eq!(session_ids(&w), vec!["B", "X", "A"]);
        assert_eq!(w.len(), 1, "no orphans");
    }

    #[test]
    fn idle_shells_match_by_cwd_and_programs_by_title() {
        let l = layout(vec![
            vec![term("~/src", "/src"), term("nvim main.rs", "/src")],
            vec![term("~/tmp", "/tmp")],
        ]);
        let nvim = Candidate {
            tty: None,
            cwd: Some("/src".into()),
            program: Some(Program {
                name: "nvim".into(),
                argv: vec!["nvim".into(), "main.rs".into()],
                claude: None,
            }),
        };
        let c = vec![
            Candidate {
                tty: None,
                cwd: Some("/tmp".into()),
                program: None,
            },
            nvim.clone(),
            Candidate {
                tty: None,
                cwd: Some("/src".into()),
                program: None,
            },
        ];
        let w = assemble(&l, c);
        let panes = &w[0].tabs[0].panes;
        assert!(panes[0].program.is_none());
        assert_eq!(panes[1].program, nvim.program);
        assert_eq!(w[0].tabs[1].panes[0].cwd, "/tmp");
    }

    #[test]
    fn unmatched_programs_land_in_an_orphan_window() {
        let l = layout(vec![vec![term("✳ beta", "/d")]]);
        let c = vec![
            claude(Some("beta"), "B", "/d"),
            claude(Some("lost"), "L", "/x"),
            Candidate {
                tty: None,
                cwd: Some("/y".into()),
                program: None,
            },
        ];
        let w = assemble(&l, c);
        assert_eq!(w.len(), 2);
        assert!(w[1].orphans);
        assert_eq!(session_ids(&w), vec!["B", "L"]);
    }

    #[test]
    fn reported_ttys_pair_exactly_and_the_rest_by_score() {
        // Same title and cwd everywhere: only the tty can tell them apart.
        let mut a = term("✳ x", "/d");
        a.tty = Some("ttys002".into());
        let b = term("✳ x", "/d");
        let mut c = term("✳ x", "/d");
        c.tty = Some("ttys001".into());
        let l = layout(vec![vec![a], vec![b], vec![c]]);
        let on = |tty: &str, sid: &str| Candidate {
            tty: Some(tty.into()),
            ..claude(Some("x"), sid, "/d")
        };
        let w = assemble(
            &l,
            vec![
                on("ttys001", "ONE"),
                on("ttys002", "TWO"),
                on("ttys003", "THREE"),
            ],
        );
        assert_eq!(session_ids(&w), vec!["TWO", "THREE", "ONE"]);
    }

    #[test]
    fn strips_status_glyphs() {
        assert_eq!(
            strip_title("✳ ameliorer-qualite-ocr"),
            "ameliorer-qualite-ocr"
        );
        assert_eq!(strip_title("◐ Claude Code"), "Claude Code");
        assert_eq!(strip_title("plain"), "plain");
    }
}
