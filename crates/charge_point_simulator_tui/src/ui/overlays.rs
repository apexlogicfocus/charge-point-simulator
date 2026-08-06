use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::text::Line;
use ratatui::widgets::{Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use super::dashboard::{MIN_HEIGHT, MIN_WIDTH};
use crate::keybindings;
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

pub(super) fn render_help(frame: &mut Frame, scroll: usize) {
    let area = frame.area();
    // The text is derived from `keybindings::SECTIONS` so it can't drift from what
    // `App::handle_key_event` and friends actually do - see `keybindings.rs`.
    let lines = keybindings::help_lines();

    // Sized to the text rather than a hardcoded row count: the popup previously clipped its
    // own last three lines, so bindings at the bottom of the list never rendered at all. +2
    // for the block's top and bottom borders, then clamped to the terminal.
    //
    // Clamped to `area.height - 2`, not `area.height`: a popup that exactly fills the terminal
    // sits flush against row 0 and the last row, so the dashboard's header and command bar -
    // which the `Clear` widget only erases *inside* the popup's own columns - show through
    // right up against the popup's border on both sides. Reserving a one-row margin (as long
    // as there's height to spare) keeps the popup visually separate from what's behind it;
    // below that, cramped is unavoidable and the scrollbar (via `needs_scrollbar`) takes over.
    let content_height = lines.len() as u16 + 2;
    let max_popup_height = area.height.saturating_sub(2).max(1);
    let popup = super::centered_rect(
        area.width.min(56),
        max_popup_height.min(content_height),
        area,
    );
    frame.render_widget(Clear, popup);

    // A `section` (see `theme::bordered_block`) only spends 2 rows on chrome (top + bottom
    // borders), so all but 2 rows of the popup are available for content.
    let visible_height = popup.height.saturating_sub(2) as usize;
    let total = lines.len();
    let needs_scrollbar = total > visible_height && visible_height > 0;

    let title = if needs_scrollbar {
        "Help (\u{2191}/\u{2193} scroll, Esc to close)"
    } else {
        "Help (Esc to close)"
    };

    let block = bordered_block(title);
    let mut text_area = block.inner(popup);
    if needs_scrollbar {
        text_area.width = text_area.width.saturating_sub(1);
    }
    frame.render_widget(block, popup);

    // Clamp so scrolling can never run past the last line and show a blank popup.
    let max_scroll = total.saturating_sub(visible_height);
    let scroll = scroll.min(max_scroll);

    let visible: Vec<Line> = lines
        .iter()
        .skip(scroll)
        .take(visible_height.max(1))
        .map(|line| Line::from(line.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(visible).style(theme::text_dim()), text_area);

    if needs_scrollbar {
        let mut state = ScrollbarState::new(max_scroll).position(scroll);
        let track = ratatui::layout::Rect {
            y: popup.y + 1,
            height: popup.height.saturating_sub(2),
            ..popup
        };
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            track,
            &mut state,
        );
    }
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
