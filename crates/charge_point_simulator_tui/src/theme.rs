//! Theme: the shared visual vocabulary for the dashboard.
//!
//! Colors are organized into a small number of deliberate tiers so future UI
//! code reaches for a semantic accessor here instead of naming a `Color` ad
//! hoc at the call site:
//!
//! - **chrome** — structural furniture: separators, rules, section titles.
//!   [`chrome()`] recedes on an unfocused panel; [`chrome_focused()`] is the
//!   same furniture brightened to brand teal on the panel that has focus.
//! - **text** — body copy, in three tiers by how important it is:
//!   [`text()`] (primary — inherits the terminal's own foreground),
//!   [`text_dim()`] (secondary — labels, units, metadata), and
//!   [`text_muted()`] (tertiary — placeholders, "(blank)", disabled
//!   entries).
//! - **accent** — [`accent()`], the one color used purely for emphasis,
//!   independent of meaning.
//! - **severity** — [`ok()`], [`warn()`], [`error()`], [`info()`], used for
//!   log levels and status messages. This is the only place meaning is
//!   color-coded, and even here every status also carries a distinct glyph
//!   (see [`connector_glyph`]/[`connection_glyph`]) so the UI stays legible
//!   without color for colorblind users and on monochrome or piped
//!   terminals.
//! - **selected** — [`selected()`], the highlight for the active row in a
//!   list or palette.
//!
//! Two constraints shape every choice above:
//!
//! 1. The user's terminal may be light or dark, and we don't get to know
//!    which. Primary body text therefore uses `Color::Reset` to inherit the
//!    terminal's own foreground rather than hardcoding white or black. Only
//!    the dimmed tiers and the semantic colors name an explicit `Color`,
//!    because those are deliberately opting out of "whatever the user's fg
//!    is."
//! 2. We do NOT use `Modifier::DIM` anywhere in this module. Terminal
//!    support for it is inconsistent — some terminals no-op it, others alter
//!    the wrong attribute — so "dim" is expressed purely through color
//!    choice (e.g. `Color::DarkGray`) rather than a text modifier. Please
//!    don't reach for `Modifier::DIM` here even though it looks tempting;
//!    it's not the portability win it appears to be.

use charge_point_simulator_core::charger::{ConnectionStatus, ConnectorStatus};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders};

/// Flowion brand teal, used as the base color for the banner and block borders.
pub const BRAND_TEAL: Color = Color::Rgb(0x14, 0xB8, 0xA6);

/// A four-sided bordered block, styled through the theme's brand accent. Reserved for
/// floating overlays (command palette, parameter prompt, help, quit confirm) that draw on
/// top of other content and need all four edges to separate themselves from what's behind
/// them - in-place panels use [`section`] instead.
pub fn bordered_block<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::bordered().title(title).border_style(accent())
}

// --- chrome: structural furniture -------------------------------------------

/// Style for separators, rules, and section titles on an unfocused panel.
/// Muted so it recedes and lets content lead.
pub fn chrome() -> Style {
    Style::new().fg(Color::DarkGray)
}

/// Style for the same chrome elements on the panel that currently has focus.
/// Brand teal draws the eye to which panel is active.
pub fn chrome_focused() -> Style {
    Style::new().fg(BRAND_TEAL)
}

// --- text: body copy, by importance -----------------------------------------

/// Normal body text. Inherits the terminal's own foreground color (light or
/// dark) instead of hardcoding one, per the module's terminal-theme-agnostic
/// constraint.
pub fn text() -> Style {
    Style::new().fg(Color::Reset)
}

/// Secondary text: labels, units, metadata. Dimmer than `text()` but still
/// comfortably readable.
pub fn text_dim() -> Style {
    Style::new().fg(Color::Gray)
}

/// Tertiary text: placeholders, "(blank)", disabled entries. The most muted
/// tier — present but clearly backgrounded.
pub fn text_muted() -> Style {
    Style::new().fg(Color::DarkGray)
}

// --- accent ------------------------------------------------------------------

/// The brand teal used for emphasis, independent of status meaning.
pub fn accent() -> Style {
    Style::new().fg(BRAND_TEAL)
}

// --- severity: log levels and status messages ---------------------------------

/// Healthy / success.
pub fn ok() -> Style {
    Style::new().fg(Color::Green)
}

/// Caution, needs attention but not broken.
///
/// No caller yet: the command bar's status messages (the only place severity is currently
/// surfaced) only ever resolve to [`ok`] or [`error`] - nothing in the app produces a
/// caution-level message today. Kept for when one does, rather than removed and
/// re-added.
#[allow(dead_code)]
pub fn warn() -> Style {
    Style::new().fg(Color::Yellow)
}

/// Broken, failed, needs intervention.
pub fn error() -> Style {
    Style::new().fg(Color::Red)
}

/// Informational, neutral-but-notable.
///
/// No caller yet, for the same reason as [`warn`] - see its doc comment.
#[allow(dead_code)]
pub fn info() -> Style {
    Style::new().fg(Color::Cyan)
}

// --- selection -----------------------------------------------------------------

/// Highlight style for the selected row in a list or command palette.
/// Uses an explicit fg/bg pair (rather than inheriting the terminal
/// foreground) because a selection highlight is a deliberate, self-contained
/// block of color that must stay legible against its own background
/// regardless of the terminal's theme.
pub fn selected() -> Style {
    Style::new().fg(Color::Black).bg(BRAND_TEAL).add_modifier(Modifier::BOLD)
}

// --- status glyph vocabulary ---------------------------------------------------
//
// Every status also gets a distinct single-character glyph so the dashboard
// stays legible without color: colorblind users and monochrome or piped
// terminals still get the full picture.

/// A single-character glyph for a connector status, for use alongside (or
/// instead of) `connector_style`'s color.
pub fn connector_glyph(status: ConnectorStatus) -> &'static str {
    match status {
        ConnectorStatus::Available => "○",
        ConnectorStatus::Occupied => "◐",
        ConnectorStatus::Charging => "●",
        ConnectorStatus::Faulted => "✕",
        ConnectorStatus::Unavailable => "⊘",
        ConnectorStatus::Reserved => "◆",
    }
}

/// A single-character glyph for a connection status, for use alongside (or
/// instead of) `connection_style`'s color.
pub fn connection_glyph(status: ConnectionStatus) -> &'static str {
    match status {
        ConnectionStatus::Booting => "◐",
        ConnectionStatus::Connected => "●",
        ConnectionStatus::Reconnecting => "↻",
        ConnectionStatus::Offline => "✕",
    }
}

/// Renders a status glyph in a fixed two-column field: the glyph followed by exactly one
/// trailing space, as a single unit that callers should always place - never the bare glyph.
///
/// Every glyph above is Unicode "Ambiguous width": the standard leaves it up to the
/// rendering environment whether it's single- or double-width, so it renders as one column
/// in most terminals but two in ones configured with CJK fonts. `ratatui`'s own width
/// accounting (via `unicode-width`) treats it as narrow, reserving exactly one buffer cell
/// for it and one for the space that follows - two cells total. In a terminal that instead
/// renders the glyph two columns wide, that extra column bleeds into the reserved space's
/// cell, which is blank anyway, so nothing meaningful is lost and every column after the
/// field still lines up. Dropping the trailing space (a "tidy this up" edit some day) would
/// remove that reserved cell and let a double-width glyph shift everything after it by one
/// column in exactly those terminals.
pub fn glyph_field(glyph: &str) -> String {
    format!("{glyph} ")
}

/// `connector_status_color` wrapped as a `Style`. `connector_status_color`
/// remains the single source of truth for the color itself so the two
/// cannot drift apart; this just adapts it to the `Style`-based API above.
pub fn connector_style(status: ConnectorStatus) -> Style {
    Style::new().fg(connector_status_color(status))
}

/// `connection_status_color` wrapped as a `Style`. See `connector_style` for
/// why this wraps rather than duplicates the color source of truth.
pub fn connection_style(status: ConnectionStatus) -> Style {
    Style::new().fg(connection_status_color(status))
}

// --- section header primitive ---------------------------------------------------

/// A block with only a top border, title embedded in that rule.
///
/// This is the header primitive the redesigned dashboard uses in place of
/// `bordered_block`'s four-sided box. A fully bordered block spends a row of
/// chrome on each of its four sides; `section` spends exactly one row (the
/// top rule) and lets content run flush to the left, right, and bottom edges
/// of its area. Across a dashboard with many panels, that reclaimed space is
/// where the redesign gets its extra vertical room from.
pub fn section<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    let style = if focused { chrome_focused() } else { chrome() };
    Block::new().borders(Borders::TOP).title(title).border_style(style)
}

pub fn connection_status_color(status: ConnectionStatus) -> Color {
    match status {
        ConnectionStatus::Booting => Color::Yellow,
        ConnectionStatus::Connected => Color::Green,
        ConnectionStatus::Reconnecting => Color::Yellow,
        ConnectionStatus::Offline => Color::Red,
    }
}

pub fn connector_status_color(status: ConnectorStatus) -> Color {
    match status {
        ConnectorStatus::Available => Color::Green,
        ConnectorStatus::Occupied => Color::Yellow,
        // Distinct from `info()` (Cyan) on purpose: "charging" is the state users look at
        // most, so it shouldn't share a hue with informational log/status severity. Blue
        // stays clear of every other connector status (Green/Yellow/Red/DarkGray/Magenta)
        // and every severity color (Green/Yellow/Red/Cyan) alike.
        ConnectorStatus::Charging => Color::LightBlue,
        ConnectorStatus::Faulted => Color::Red,
        ConnectorStatus::Unavailable => Color::DarkGray,
        ConnectorStatus::Reserved => Color::Magenta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;
    use ratatui::widgets::Borders;

    // --- new semantic style accessors -------------------------------------

    #[test]
    fn chrome_and_chrome_focused_differ_and_focused_is_brand_teal() {
        assert_ne!(chrome(), chrome_focused());
        assert_eq!(chrome_focused().fg, Some(BRAND_TEAL));
    }

    #[test]
    fn text_inherits_the_terminal_foreground_instead_of_hardcoding_one() {
        assert_eq!(text().fg, Some(Color::Reset));
    }

    #[test]
    fn text_tiers_are_distinct() {
        assert_ne!(text(), text_dim());
        assert_ne!(text_dim(), text_muted());
        assert_ne!(text(), text_muted());
    }

    #[test]
    fn accent_is_brand_teal() {
        assert_eq!(accent().fg, Some(BRAND_TEAL));
    }

    #[test]
    fn severity_styles_are_distinct() {
        let styles = [ok(), warn(), error(), info()];
        let mut fgs: Vec<Option<Color>> = styles.iter().map(|s| s.fg).collect();
        fgs.sort_by_key(|c| format!("{c:?}"));
        fgs.dedup();
        assert_eq!(fgs.len(), styles.len());
    }

    #[test]
    fn selected_style_is_distinguishable_from_plain_text() {
        assert_ne!(selected(), text());
    }

    #[test]
    fn no_style_in_the_palette_uses_the_dim_modifier() {
        let styles = [
            chrome(),
            chrome_focused(),
            text(),
            text_dim(),
            text_muted(),
            accent(),
            ok(),
            warn(),
            error(),
            info(),
            selected(),
        ];
        for style in styles {
            assert!(
                !style.add_modifier.contains(Modifier::DIM),
                "Modifier::DIM is unsupported in some terminals; dim with color instead"
            );
        }
    }

    // --- status glyph vocabulary --------------------------------------------

    #[test]
    fn every_connector_status_maps_to_a_distinct_glyph() {
        let statuses = [
            ConnectorStatus::Available,
            ConnectorStatus::Occupied,
            ConnectorStatus::Charging,
            ConnectorStatus::Faulted,
            ConnectorStatus::Unavailable,
            ConnectorStatus::Reserved,
        ];
        let glyphs: Vec<&str> = statuses.iter().copied().map(connector_glyph).collect();
        let mut unique = glyphs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), glyphs.len());
    }

    #[test]
    fn connector_glyphs_are_single_characters() {
        let statuses = [
            ConnectorStatus::Available,
            ConnectorStatus::Occupied,
            ConnectorStatus::Charging,
            ConnectorStatus::Faulted,
            ConnectorStatus::Unavailable,
            ConnectorStatus::Reserved,
        ];
        for status in statuses {
            assert_eq!(
                connector_glyph(status).chars().count(),
                1,
                "glyph for {status:?} is not a single character"
            );
        }
    }

    #[test]
    fn connection_glyphs_are_single_characters() {
        let statuses = [
            ConnectionStatus::Booting,
            ConnectionStatus::Connected,
            ConnectionStatus::Reconnecting,
            ConnectionStatus::Offline,
        ];
        for status in statuses {
            assert_eq!(
                connection_glyph(status).chars().count(),
                1,
                "glyph for {status:?} is not a single character"
            );
        }
    }

    #[test]
    fn glyph_field_is_a_fixed_two_column_field() {
        // The field is always "glyph + one space", i.e. exactly two `char`s - see
        // `glyph_field`'s doc comment for why that width must stay fixed regardless of how
        // wide the terminal actually renders the (Unicode "Ambiguous width") glyph.
        for glyph in [
            connector_glyph(ConnectorStatus::Charging),
            connector_glyph(ConnectorStatus::Faulted),
            connection_glyph(ConnectionStatus::Connected),
            connection_glyph(ConnectionStatus::Reconnecting),
        ] {
            assert_eq!(
                glyph_field(glyph).chars().count(),
                2,
                "glyph field for {glyph:?} is not a fixed two-column field"
            );
        }
    }

    // --- anti-drift: *_style wraps the existing *_color source of truth ----

    #[test]
    fn connector_style_agrees_with_connector_status_color_for_every_status() {
        let statuses = [
            ConnectorStatus::Available,
            ConnectorStatus::Occupied,
            ConnectorStatus::Charging,
            ConnectorStatus::Faulted,
            ConnectorStatus::Unavailable,
            ConnectorStatus::Reserved,
        ];
        for status in statuses {
            assert_eq!(
                connector_style(status).fg,
                Some(connector_status_color(status)),
                "connector_style drifted from connector_status_color for {status:?}"
            );
        }
    }

    #[test]
    fn connection_style_agrees_with_connection_status_color_for_every_status() {
        let statuses = [
            ConnectionStatus::Booting,
            ConnectionStatus::Connected,
            ConnectionStatus::Reconnecting,
            ConnectionStatus::Offline,
        ];
        for status in statuses {
            assert_eq!(
                connection_style(status).fg,
                Some(connection_status_color(status)),
                "connection_style drifted from connection_status_color for {status:?}"
            );
        }
    }

    // --- section() header primitive -----------------------------------------

    #[test]
    fn section_renders_only_a_top_border() {
        let block = section("Title", false);
        assert_eq!(block.inner(ratatui::layout::Rect::new(0, 0, 10, 10)).y, 1);
        // A block with only Borders::TOP takes exactly one row off the top,
        // and none off the left/right/bottom.
        let inner = block.inner(ratatui::layout::Rect::new(0, 0, 10, 10));
        assert_eq!(inner, ratatui::layout::Rect::new(0, 1, 10, 9));
    }

    #[test]
    fn section_border_style_responds_to_focus() {
        let unfocused = section("Title", false);
        let focused = section("Title", true);
        assert_eq!(unfocused.to_owned(), Block::new().borders(Borders::TOP).title("Title").border_style(chrome()));
        assert_eq!(focused.to_owned(), Block::new().borders(Borders::TOP).title("Title").border_style(chrome_focused()));
    }

    #[test]
    fn healthy_connection_states_read_as_green_or_cautionary_yellow() {
        assert_eq!(
            connection_status_color(ConnectionStatus::Connected),
            Color::Green
        );
        assert_eq!(
            connection_status_color(ConnectionStatus::Booting),
            Color::Yellow
        );
        assert_eq!(
            connection_status_color(ConnectionStatus::Reconnecting),
            Color::Yellow
        );
    }

    #[test]
    fn offline_connection_reads_as_red() {
        assert_eq!(
            connection_status_color(ConnectionStatus::Offline),
            Color::Red
        );
    }

    #[test]
    fn faulted_connector_reads_as_red_and_available_as_green() {
        assert_eq!(
            connector_status_color(ConnectorStatus::Faulted),
            Color::Red
        );
        assert_eq!(
            connector_status_color(ConnectorStatus::Available),
            Color::Green
        );
    }

    #[test]
    fn charging_connector_color_is_distinct_from_the_info_severity_color() {
        // Both used to be Cyan; "charging" is the most-watched state and shouldn't share a
        // hue with informational log/status severity.
        assert_ne!(
            connector_status_color(ConnectorStatus::Charging),
            info().fg.unwrap()
        );
    }

    #[test]
    fn every_connector_status_maps_to_a_distinct_color() {
        let statuses = [
            ConnectorStatus::Available,
            ConnectorStatus::Occupied,
            ConnectorStatus::Charging,
            ConnectorStatus::Faulted,
            ConnectorStatus::Unavailable,
            ConnectorStatus::Reserved,
        ];
        let colors: Vec<Color> = statuses.iter().copied().map(connector_status_color).collect();
        let mut unique = colors.clone();
        unique.sort_by_key(|c| format!("{c:?}"));
        unique.dedup();
        assert_eq!(unique.len(), colors.len());
    }
}
