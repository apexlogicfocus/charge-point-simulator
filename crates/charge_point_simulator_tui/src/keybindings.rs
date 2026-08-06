//! A single source of truth for every keybinding the app has.
//!
//! `App::handle_key_event` and its per-screen handlers (`handle_pick_charger_key`,
//! `handle_dashboard_key`, `handle_log_filter_key`, `handle_help_key`,
//! `handle_command_palette_key`, `handle_parameter_prompt_key`) are the actual behavior; this
//! table just documents them so the help overlay and the dashboard hint can't drift from what
//! the code does. If you add or change a binding in `app.rs`, update it here too.

/// One key (or key combo) and what it does.
pub struct Binding {
    pub keys: &'static str,
    pub description: &'static str,
}

/// A named group of bindings, rendered as one block in the help overlay.
pub struct Section {
    pub title: &'static str,
    pub bindings: &'static [Binding],
}

pub const SECTIONS: &[Section] = &[
    Section {
        title: "Global",
        bindings: &[
            Binding {
                keys: "q",
                description: "quit (confirm)",
            },
            Binding {
                keys: "?",
                description: "toggle this help",
            },
        ],
    },
    Section {
        title: "Charger picker",
        bindings: &[
            Binding {
                keys: "\u{2191}/\u{2193}",
                description: "move selection",
            },
            Binding {
                keys: "type",
                description: "filter by charger id",
            },
            Binding {
                keys: "Enter",
                description: "select charger",
            },
            Binding {
                keys: "Esc",
                description: "clear filter (or quit)",
            },
        ],
    },
    Section {
        title: "Dashboard",
        bindings: &[
            Binding {
                keys: "Esc",
                description: "back to picker",
            },
            Binding {
                keys: "\u{2191}/\u{2193}",
                description: "focus connector",
            },
            Binding {
                keys: "Tab/\u{2190}/\u{2192}",
                description: "focus EVSE",
            },
            Binding {
                keys: "Ctrl+K / c",
                description: "open command palette",
            },
        ],
    },
    Section {
        title: "Logs",
        bindings: &[
            Binding {
                keys: "PgUp/PgDn",
                description: "scroll",
            },
            Binding {
                keys: "g / G",
                description: "jump to oldest / newest",
            },
            Binding {
                keys: "/",
                description: "filter (Esc clears)",
            },
            Binding {
                keys: "l",
                description: "cycle level threshold",
            },
        ],
    },
    Section {
        title: "Help",
        bindings: &[
            Binding {
                keys: "\u{2191}/\u{2193}",
                description: "scroll",
            },
            Binding {
                keys: "Esc",
                description: "close",
            },
        ],
    },
];

/// Widest `keys` column across every section, so the description column lines up regardless of
/// which section is longest. Computed from the table rather than hardcoded, so a longer key
/// string can't silently break alignment.
fn key_column_width() -> usize {
    SECTIONS
        .iter()
        .flat_map(|section| section.bindings.iter())
        .map(|binding| binding.keys.chars().count())
        .max()
        .unwrap_or(0)
}

/// The help overlay's body: one line per section title, a blank line, then each binding as an
/// aligned `keys` column + description, with a blank line between sections.
pub fn help_lines() -> Vec<String> {
    let width = key_column_width();
    let mut lines = Vec::new();
    for (i, section) in SECTIONS.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        lines.push(section.title.to_string());
        for binding in section.bindings {
            lines.push(format!(
                " {:width$}  {}",
                binding.keys,
                binding.description,
                width = width
            ));
        }
    }
    lines
}

/// The dashboard command bar's one-line hint, built from the table.
///
/// Deliberate transition step: this is kept byte-for-byte equal to the string
/// `dashboard.rs` used to hardcode, so wiring it in doesn't churn a dozen snapshot goldens.
/// It is not a promise that the wording stays fixed forever - once callers move off the old
/// golden strings this can reflow to whatever fits the command bar best.
pub fn dashboard_hint() -> String {
    "q: quit  Esc: back  \u{2191}/\u{2193}: connector  Tab/\u{2190}/\u{2192}: EVSE  /: filter  \
     l: level  c: command  ?: help"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_section_is_non_empty() {
        for section in SECTIONS {
            assert!(
                !section.bindings.is_empty(),
                "section {:?} has no bindings",
                section.title
            );
        }
    }

    #[test]
    fn key_column_is_aligned() {
        let width = key_column_width();
        let lines = help_lines();
        let mut idx = 0;
        for section in SECTIONS {
            idx += 1; // title line
            for binding in section.bindings {
                let expected = format!(" {:width$}  {}", binding.keys, binding.description, width = width);
                assert_eq!(
                    lines[idx], expected,
                    "binding line for {:?} is not aligned to the computed width",
                    binding.keys
                );
                idx += 1;
            }
            idx += 1; // blank separator line (present after every section, harmless past the end)
        }
    }

    #[test]
    fn dashboard_hint_matches_expected_string() {
        assert_eq!(
            dashboard_hint(),
            "q: quit  Esc: back  \u{2191}/\u{2193}: connector  Tab/\u{2190}/\u{2192}: EVSE  \
             /: filter  l: level  c: command  ?: help"
        );
    }

    #[test]
    fn help_lines_mention_every_binding() {
        let lines = help_lines().join("\n");
        for section in SECTIONS {
            assert!(lines.contains(section.title), "missing section title {:?}", section.title);
            for binding in section.bindings {
                assert!(
                    lines.contains(binding.keys),
                    "missing keys {:?}",
                    binding.keys
                );
                assert!(
                    lines.contains(binding.description),
                    "missing description {:?}",
                    binding.description
                );
            }
        }
    }
}
