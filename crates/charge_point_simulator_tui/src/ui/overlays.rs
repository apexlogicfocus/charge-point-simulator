use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::widgets::{Clear, Paragraph};

use super::dashboard::{MIN_HEIGHT, MIN_WIDTH};
use crate::theme::{self, bordered_block};

pub(super) fn render_too_small(frame: &mut Frame) {
    let area = frame.area();
    frame.render_widget(
        Paragraph::new(format!(
            "Terminal too small.\nResize to at least {}x{}.",
            MIN_WIDTH, MIN_HEIGHT
        ))
        .style(theme::text())
        .alignment(Alignment::Center),
        area,
    );
}

pub(super) fn render_help(frame: &mut Frame) {
    let area = frame.area();
    // The "Dashboard" section's focus line packs both `↑`/`↓` (connector-level, flows across
    // EVSE boundaries) and `Tab`/`←`/`→` (EVSE-level jumps) onto one line rather than two, even
    // though they're meaningfully different - see `App::select_next_connector`/
    // `App::select_next_evse`.
    let text = "Global\n\
         \u{20}q            quit (confirm)\n\
         \u{20}?            toggle this help\n\n\
         Charger picker\n\
         \u{20}\u{2191}/\u{2193}        move selection\n\
         \u{20}type         filter by charger id\n\
         \u{20}Enter        select charger\n\
         \u{20}Esc          clear filter (or quit)\n\n\
         Dashboard\n\
         \u{20}Esc          back to picker\n\
         \u{20}\u{2191}/\u{2193} \u{2190}/\u{2192}/Tab  focus connector / EVSE\n\
         \u{20}c            open command palette\n\n\
         Logs\n\
         \u{20}PgUp/PgDn    scroll\n\
         \u{20}g / G        jump to oldest / newest\n\
         \u{20}/            filter (Esc clears)\n\
         \u{20}l            cycle level threshold";

    // Sized to the text rather than a hardcoded 14 rows: the popup previously clipped its own
    // last three lines, so the bindings at the bottom of the list - now including every log
    // binding this phase added - never rendered at all. +2 for the block's top and bottom
    // borders, then clamped to the terminal.
    let content_height = text.lines().count() as u16 + 2;
    let popup = super::centered_rect(area.width.min(56), area.height.min(content_height), area);
    frame.render_widget(Clear, popup);

    frame.render_widget(
        Paragraph::new(text)
            .style(theme::text_dim())
            .block(bordered_block("Help (Esc to close)")),
        popup,
    );
}

pub(super) fn render_quit_confirm(frame: &mut Frame) {
    let area = frame.area();
    let popup = super::centered_rect(area.width.min(34), 3, area);
    frame.render_widget(Clear, popup);

    frame.render_widget(
        Paragraph::new("Quit the simulator? (y/n)")
            .style(theme::text())
            .alignment(Alignment::Center)
            .block(bordered_block("Quit?")),
        popup,
    );
}
