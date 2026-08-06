use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use super::view::{PaletteView, ParameterPromptView};
use crate::theme::{self, bordered_block};

/// Wider than the old 50 columns: each row now carries a label *and* a description, and the
/// header line names the connector every command would act on.
const PALETTE_WIDTH: u16 = 64;

/// The palette popup's own sub-regions, split out from its outer popup rect by
/// [`palette_layout`]: the target line, the filter field, and the command list - shared by
/// [`render_command_palette`] and mouse hit-testing (see [`command_index_at`]) so the two can
/// never disagree about where the list actually is.
struct PaletteLayout {
    target: Rect,
    filter: Rect,
    list: Rect,
}

/// The popup's outer [`Rect`], centered in `area` and sized to fit `command_count` rows (see
/// [`render_command_palette`]'s original inline computation, factored out here so mouse
/// hit-testing can recompute the exact same popup a redraw would place).
pub(crate) fn palette_popup_rect(area: Rect, command_count: usize) -> Rect {
    let width = area.width.min(PALETTE_WIDTH);
    let height = (command_count as u16 + 6).max(7).min(area.height);
    super::centered_rect(width, height, area)
}

/// The command list's own rect within the popup - the piece of [`palette_layout`] mouse
/// hit-testing actually needs (see [`command_index_at`]), without exposing the private
/// [`PaletteLayout`] struct itself outside this module.
pub(crate) fn palette_list_area(popup: Rect) -> Rect {
    palette_layout(popup).list
}

fn palette_layout(popup: Rect) -> PaletteLayout {
    let [target, filter, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(popup);
    PaletteLayout {
        target,
        filter,
        list,
    }
}

/// Which command row, if any, `(mouse_col, mouse_row)` lands on within the palette's command
/// list - `list_area` is [`PaletteLayout::list`], the same rect [`render_command_palette`] hands
/// its `List` widget, which draws a full border around it (unlike the dashboard's tree/log
/// panes, which only spend a top row on chrome - see `theme::bordered_block` vs.
/// `theme::section`), so the first row of content sits one column *and* one row in from the
/// list area's corner. `None` off the list, on its border, or past the last real command.
///
/// This assumes every row is visible without the list's own internal scrolling ever having
/// kicked in - true for how large this app's command lists get in practice, but see the roadmap
/// note (Phase 7 mouse support) if that stops holding: `ratatui::widgets::ListState` doesn't
/// expose the scroll offset it settles on, so a palette long enough to scroll would need the
/// same manual-`Paragraph` treatment `dashboard.rs`'s tree/log panes already use before a click
/// could be mapped precisely.
pub(crate) fn command_index_at(
    list_area: Rect,
    mouse_col: u16,
    mouse_row: u16,
    command_count: usize,
) -> Option<usize> {
    let content = Rect {
        x: list_area.x + 1,
        y: list_area.y + 1,
        width: list_area.width.saturating_sub(2),
        height: list_area.height.saturating_sub(2),
    };
    if !content.contains(Position::new(mouse_col, mouse_row)) {
        return None;
    }
    let index = (mouse_row - content.y) as usize;
    (index < command_count).then_some(index)
}

pub(super) fn render_command_palette(frame: &mut Frame, view: &PaletteView) {
    let area = frame.area();
    let popup = palette_popup_rect(area, view.commands.len());

    frame.render_widget(Clear, popup);

    let PaletteLayout {
        target: target_area,
        filter: filter_area,
        list: list_area,
    } = palette_layout(popup);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_popup_rect_is_centered_and_sized_for_the_command_count() {
        let area = Rect::new(0, 0, 120, 40);
        let popup = palette_popup_rect(area, 3);

        assert_eq!(popup.width, PALETTE_WIDTH);
        assert_eq!(popup.height, 9); // 3 commands + 6
        assert_eq!(popup.x, (area.width - popup.width) / 2);
        assert_eq!(popup.y, (area.height - popup.height) / 2);
    }

    #[test]
    fn command_index_at_finds_the_row_under_the_click() {
        let popup = palette_popup_rect(Rect::new(0, 0, 120, 40), 3);
        let list_area = palette_layout(popup).list;

        // Row 0 sits one column and one row inside the list's bordered block.
        let top_left = (list_area.x + 1, list_area.y + 1);
        assert_eq!(
            command_index_at(list_area, top_left.0, top_left.1, 3),
            Some(0)
        );
        assert_eq!(
            command_index_at(list_area, top_left.0, top_left.1 + 1, 3),
            Some(1)
        );
    }

    #[test]
    fn command_index_at_is_none_on_the_border_or_past_the_last_command() {
        let popup = palette_popup_rect(Rect::new(0, 0, 120, 40), 2);
        let list_area = palette_layout(popup).list;

        assert_eq!(
            command_index_at(list_area, list_area.x, list_area.y, 2),
            None
        ); // border
        assert_eq!(
            command_index_at(list_area, list_area.x + 1, list_area.y + 1 + 2, 2),
            None
        ); // past the last command's row
    }

    #[test]
    fn command_index_at_is_none_with_no_commands() {
        let popup = palette_popup_rect(Rect::new(0, 0, 120, 40), 0);
        let list_area = palette_layout(popup).list;

        assert_eq!(
            command_index_at(list_area, list_area.x + 1, list_area.y + 1, 0),
            None
        );
    }
}
