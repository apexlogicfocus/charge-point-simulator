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
    let popup = super::centered_rect(area.width.min(56), area.height.min(14), area);
    frame.render_widget(Clear, popup);

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
         \u{20}\u{2190}/\u{2192} or Tab   focus EVSE\n\
         \u{20}PgUp/PgDn    scroll logs\n\
         \u{20}c            open command palette";

    frame.render_widget(
        Paragraph::new(text).style(theme::text_dim()).block(bordered_block("Help (Esc to close)")),
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
