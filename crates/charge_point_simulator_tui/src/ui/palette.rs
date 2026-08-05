use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use crate::app::App;
use crate::theme::{self, bordered_block};
use charge_point_simulator_core::charger::Command;

pub(super) fn render_command_palette(frame: &mut Frame, app: &App) {
    let commands = app.palette_commands();
    let area = frame.area();
    let width = area.width.min(50);
    let height = (commands.len() as u16 + 5).max(6).min(area.height);
    let popup = super::centered_rect(width, height, area);

    frame.render_widget(Clear, popup);

    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(popup);

    frame.render_widget(
        Paragraph::new(Line::styled(app.command_palette_filter.value(), theme::text()))
            .block(bordered_block("Filter")),
        filter_area,
    );
    frame.set_cursor_position((
        filter_area.x + 1 + app.command_palette_filter.cursor() as u16,
        filter_area.y + 1,
    ));

    let items: Vec<ListItem> = if commands.is_empty() {
        vec![ListItem::new(Line::styled("no commands match", theme::text_muted()))]
    } else {
        commands
            .iter()
            .map(|command| ListItem::new(Line::styled(command.label(), theme::text())))
            .collect()
    };

    let list = List::new(items)
        .block(bordered_block("Command"))
        .highlight_style(theme::selected())
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !commands.is_empty() {
        state.select(Some(app.command_palette_selected));
    }

    frame.render_stateful_widget(list, list_area, &mut state);
}

pub(super) fn render_parameter_prompt(frame: &mut Frame, app: &App, command: Command) {
    let Some(parameter) = command.parameter() else {
        return;
    };
    let area = frame.area();
    let popup = super::centered_rect(area.width.min(50), 3, area);
    frame.render_widget(Clear, popup);

    frame.render_widget(
        Paragraph::new(Line::styled(app.parameter_field.value(), theme::text()))
            .block(bordered_block(parameter.label())),
        popup,
    );
    frame.set_cursor_position((
        popup.x + 1 + app.parameter_field.cursor() as u16,
        popup.y + 1,
    ));
}
