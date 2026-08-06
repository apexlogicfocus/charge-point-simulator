use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use super::view::{PaletteView, ParameterPromptView};
use crate::theme::{self, bordered_block};

/// Wider than the old 50 columns: each row now carries a label *and* a description, and the
/// header line names the connector every command would act on.
const PALETTE_WIDTH: u16 = 64;

pub(super) fn render_command_palette(frame: &mut Frame, view: &PaletteView) {
    let area = frame.area();
    let width = area.width.min(PALETTE_WIDTH);
    let height = (view.commands.len() as u16 + 6).max(7).min(area.height);
    let popup = super::centered_rect(width, height, area);

    frame.render_widget(Clear, popup);

    let [target_area, filter_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(popup);

    // The target line answers "what will Enter actually do?" before Enter is pressed - the
    // palette used to give no indication which connector it was aimed at.
    let target_line = match &view.target {
        Some(target) => Line::from(vec![
            Span::styled(" target ", theme::text_dim()),
            Span::styled(target.as_str(), theme::accent()),
            Span::styled("  (Tab to retarget)", theme::text_muted()),
        ]),
        None => Line::styled(" no connector targeted", theme::text_muted()),
    };
    frame.render_widget(Paragraph::new(target_line), target_area);

    frame.render_widget(
        Paragraph::new(Line::styled(view.filter, theme::text())).block(bordered_block("Filter")),
        filter_area,
    );
    frame.set_cursor_position((filter_area.x + 1 + view.cursor as u16, filter_area.y + 1));

    let items: Vec<ListItem> = if view.commands.is_empty() {
        vec![ListItem::new(Line::styled(
            "no commands match",
            theme::text_muted(),
        ))]
    } else {
        // The label column is sized from the widest label actually listed, so descriptions
        // line up without a hardcoded width that a renamed command could silently break.
        let label_width = view
            .commands
            .iter()
            .map(|command| command.label().chars().count())
            .max()
            .unwrap_or(0);
        view.commands
            .iter()
            .map(|command| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:<label_width$}  ", command.label()),
                        theme::text(),
                    ),
                    Span::styled(command.description(), theme::text_dim()),
                ]))
            })
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
    // 1 row for the target line, 3 for the bordered field, 1 for the error - the error row is
    // always allotted rather than appearing and disappearing, so the field doesn't jump under
    // the cursor the moment a value is rejected.
    let popup = super::centered_rect(area.width.min(PALETTE_WIDTH), 5, area);
    frame.render_widget(Clear, popup);

    let [target_area, field_area, error_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(popup);

    let target_line = match &view.target {
        Some(target) => Line::from(vec![
            Span::styled(" target ", theme::text_dim()),
            Span::styled(target.as_str(), theme::accent()),
        ]),
        None => Line::styled(" no connector targeted", theme::text_muted()),
    };
    frame.render_widget(Paragraph::new(target_line), target_area);

    // An empty field shows an example value as a muted placeholder - the title already says
    // what the parameter is, so the placeholder's job is to show what one looks like.
    let field_line = if view.value.is_empty() {
        Line::styled(parameter.placeholder(), theme::text_muted())
    } else {
        Line::styled(view.value, theme::text())
    };
    frame.render_widget(
        Paragraph::new(field_line).block(bordered_block(parameter.label())),
        field_area,
    );
    frame.set_cursor_position((field_area.x + 1 + view.cursor as u16, field_area.y + 1));

    if let Some(error) = view.error {
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {error}"), theme::error())),
            error_area,
        );
    }
}
