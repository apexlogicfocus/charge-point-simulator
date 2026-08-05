use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use super::view::{ParameterPromptView, PaletteView};
use crate::theme::{self, bordered_block};

pub(super) fn render_command_palette(frame: &mut Frame, view: &PaletteView) {
    let area = frame.area();
    let width = area.width.min(50);
    let height = (view.commands.len() as u16 + 5).max(6).min(area.height);
    let popup = super::centered_rect(width, height, area);

    frame.render_widget(Clear, popup);

    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(popup);

    frame.render_widget(
        Paragraph::new(Line::styled(view.filter, theme::text())).block(bordered_block("Filter")),
        filter_area,
    );
    frame.set_cursor_position((filter_area.x + 1 + view.cursor as u16, filter_area.y + 1));

    let items: Vec<ListItem> = if view.commands.is_empty() {
        vec![ListItem::new(Line::styled("no commands match", theme::text_muted()))]
    } else {
        view.commands
            .iter()
            .map(|command| ListItem::new(Line::styled(command.label(), theme::text())))
            .collect()
    };

    let list = List::new(items)
        .block(bordered_block("Command"))
        .highlight_style(theme::selected())
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !view.commands.is_empty() {
        state.select(Some(view.selected));
    }

    frame.render_stateful_widget(list, list_area, &mut state);
}

pub(super) fn render_parameter_prompt(frame: &mut Frame, view: &ParameterPromptView) {
    let Some(parameter) = view.command.parameter() else {
        return;
    };
    let area = frame.area();
    let popup = super::centered_rect(area.width.min(50), 3, area);
    frame.render_widget(Clear, popup);

    frame.render_widget(
        Paragraph::new(Line::styled(view.value, theme::text())).block(bordered_block(parameter.label())),
        popup,
    );
    frame.set_cursor_position((popup.x + 1 + view.cursor as u16, popup.y + 1));
}
