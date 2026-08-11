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
        title: "Connection setup",
        bindings: &[
            Binding {
                keys: "Tab/\u{2191}/\u{2193}",
                description: "move between fields",
            },
            Binding {
                keys: "Ctrl+R",
                description: "reveal/hide password",
            },
            Binding {
                keys: "PgUp/PgDn",
                description: "cycle recent URLs",
            },
            Binding {
                keys: "Enter",
                description: "connect",
            },
            Binding {
                keys: "Esc",
                description: "cancel",
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
                description: "commands + hardware actions",
            },
            Binding {
                keys: "d",
                description: "toggle V2G discharge (if declared)",
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
            Binding {
                keys: "Ctrl+L",
                description: "clear logs",
            },
            Binding {
                keys: "y",
                description: "copy focused log line",
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

/// One entry in the dashboard hint, with a priority for which entries get dropped first when
/// the command bar is too narrow to show all of them - lowest priority dropped first.
///
/// Mirrors `header_segments`'s approach in `ui/dashboard.rs`: rather than truncating the
/// rendered string mid-word (which is how "?: help" - the entry that advertises the help
/// overlay - used to get cut off at 80 columns), whole entries are dropped from the least
/// essential end until what remains fits.
const HINT_ENTRIES: &[(&str, u8)] = &[
    ("q: quit", 3),
    ("Esc: back", 3),
    ("\u{2191}/\u{2193}: connector", 1),
    ("Tab/\u{2190}/\u{2192}: EVSE", 1),
    ("/: filter", 2),
    ("l: level", 1),
    // Lower priority than the rest: these are the first to go when the terminal is narrow,
    // below even the navigation hints - useful, but the least essential entries here. `d` joins
    // them because it only does anything on a charger that declares bidirectional power, which
    // most don't; the help overlay still documents it at any width.
    ("Ctrl+L: clear logs", 0),
    ("y: copy log", 0),
    ("d: V2G", 0),
    ("c: command", 2),
    ("?: help", 3),
];

const HINT_SEPARATOR: &str = "  ";

/// Joins `entries` (already filtered to whatever priority tier fits) with the hint's separator.
fn join_hint(entries: impl Iterator<Item = &'static str>) -> String {
    entries.collect::<Vec<_>>().join(HINT_SEPARATOR)
}

/// The dashboard command bar's hint, shortened by priority to fit `width` columns.
///
/// Drops the lowest-priority entries first (see `HINT_ENTRIES`), one priority tier at a time,
/// until the joined string fits - the same tiered-fallback shape `header_segments` uses for the
/// header line. If even the highest-priority tier alone doesn't fit, that tier is returned
/// as-is rather than cut mid-word: a slightly overflowing hint is preferable to leaving "?: help"
/// visually truncated.
pub fn dashboard_hint_for_width(width: usize) -> String {
    let min_priority = HINT_ENTRIES.iter().map(|(_, p)| *p).min().unwrap_or(0);
    let max_priority = HINT_ENTRIES.iter().map(|(_, p)| *p).max().unwrap_or(0);
    let mut narrowest = join_hint(HINT_ENTRIES.iter().map(|(text, _)| *text));
    for threshold in min_priority..=max_priority {
        let candidate = join_hint(
            HINT_ENTRIES
                .iter()
                .filter(|(_, p)| *p >= threshold)
                .map(|(text, _)| *text),
        );
        if candidate.chars().count() <= width {
            return candidate;
        }
        narrowest = candidate;
    }
    narrowest
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
                let expected = format!(
                    " {:width$}  {}",
                    binding.keys,
                    binding.description,
                    width = width
                );
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
    fn hint_for_width_returns_the_full_hint_when_it_fits() {
        assert_eq!(
            dashboard_hint_for_width(200),
            "q: quit  Esc: back  \u{2191}/\u{2193}: connector  Tab/\u{2190}/\u{2192}: EVSE  \
             /: filter  l: level  Ctrl+L: clear logs  y: copy log  d: V2G  c: command  ?: help"
        );
    }

    #[test]
    fn hint_for_width_drops_whole_entries_instead_of_truncating_mid_word() {
        let hint = dashboard_hint_for_width(80);
        assert!(hint.chars().count() <= 80);
        // The very entry that advertises the help overlay must never be the one dropped, and
        // must never be cut mid-word - that was the bug this function fixes.
        assert!(hint.ends_with("?: help"));
        assert!(hint.starts_with("q: quit"));
    }

    #[test]
    fn hint_for_width_never_exceeds_width_once_only_the_top_priority_tier_remains() {
        // The top tier ("q: quit  Esc: back  ?: help") is 27 columns; anything at or above
        // that width must fit exactly.
        let hint = dashboard_hint_for_width(27);
        assert_eq!(hint.chars().count(), 27);
        assert_eq!(hint, "q: quit  Esc: back  ?: help");
    }

    #[test]
    fn hint_for_width_falls_back_to_the_narrowest_tier_when_even_that_overflows() {
        // Pathologically small width: nothing fits, but we still return the narrowest tier
        // rather than an empty string or a mid-word cut.
        let hint = dashboard_hint_for_width(5);
        assert_eq!(hint, "q: quit  Esc: back  ?: help");
    }

    #[test]
    fn help_lines_mention_every_binding() {
        let lines = help_lines().join("\n");
        for section in SECTIONS {
            assert!(
                lines.contains(section.title),
                "missing section title {:?}",
                section.title
            );
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
