use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::text_field::TextField;
use crate::theme;

/// The three input rows of the connection setup screen, plus a hint footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionSetupLayout {
    pub csms_url: Rect,
    pub ocpp_identity: Rect,
    pub password: Rect,
    pub hint: Rect,
}

// 2, not 3: each field is now a `theme::section` (1 row of chrome, the top rule) rather than
// a 4-sided `bordered_block` (2 rows of chrome), so it needs 1 fewer row to show the same
// single content line.
const FIELD_HEIGHT: u16 = 2;
const HINT_HEIGHT: u16 = 1;

/// Stacks the three bordered input fields top to bottom with a hint line at the
/// bottom, centered in whatever space is available.
pub fn connection_setup_layout(area: Rect) -> ConnectionSetupLayout {
    let content_height = FIELD_HEIGHT * 3 + HINT_HEIGHT;
    let [_, content, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(content_height),
        Constraint::Fill(1),
    ])
    .areas(area);

    let [csms_url, ocpp_identity, password, hint] = Layout::vertical([
        Constraint::Length(FIELD_HEIGHT),
        Constraint::Length(FIELD_HEIGHT),
        Constraint::Length(FIELD_HEIGHT),
        Constraint::Length(HINT_HEIGHT),
    ])
    .areas(content);

    ConnectionSetupLayout {
        csms_url,
        ocpp_identity,
        password,
        hint,
    }
}

pub(super) fn render(frame: &mut Frame, app: &App) {
    let layout = connection_setup_layout(frame.area());

    let fields: [(&str, &TextField, bool, Rect); 3] = [
        (
            "CSMS URL",
            &app.connection_csms_url,
            app.connection_focused_field == 0,
            layout.csms_url,
        ),
        (
            "OCPP Identity",
            &app.connection_ocpp_identity,
            app.connection_focused_field == 1,
            layout.ocpp_identity,
        ),
        (
            "Password",
            &app.connection_password,
            app.connection_focused_field == 2,
            layout.password,
        ),
    ];

    for (title, field, focused, area) in fields {
        let display_value = if title == "Password" {
            "*".repeat(field.value().chars().count())
        } else {
            field.value().to_string()
        };
        // Unlike the dashboard's panels, this screen already tracks genuine per-field focus
        // (`connection_focused_field`), so `focused` here is real, not a placeholder - the
        // focused field's section rule lights up in `chrome_focused()`'s brand teal.
        frame.render_widget(
            Paragraph::new(Line::styled(display_value, theme::text())).block(theme::section(title, focused)),
            area,
        );
        if focused {
            frame.set_cursor_position((area.x + field.cursor() as u16, area.y + 1));
        }
    }

    frame.render_widget(
        Paragraph::new(Line::styled(
            "Tab: next field  Enter: connect  Esc: cancel",
            theme::text_dim(),
        )),
        layout.hint,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    #[test]
    fn fields_stack_top_to_bottom_in_order() {
        let layout = connection_setup_layout(area(80, 30));

        assert_eq!(layout.ocpp_identity.y, layout.csms_url.bottom());
        assert_eq!(layout.password.y, layout.ocpp_identity.bottom());
        assert_eq!(layout.hint.y, layout.password.bottom());
    }

    #[test]
    fn fields_use_the_configured_height() {
        let layout = connection_setup_layout(area(80, 30));

        assert_eq!(layout.csms_url.height, FIELD_HEIGHT);
        assert_eq!(layout.ocpp_identity.height, FIELD_HEIGHT);
        assert_eq!(layout.password.height, FIELD_HEIGHT);
        assert_eq!(layout.hint.height, HINT_HEIGHT);
    }

    #[test]
    fn content_is_vertically_centered_when_space_allows() {
        let outer = area(80, 40);
        let layout = connection_setup_layout(outer);
        let content_height = FIELD_HEIGHT * 3 + HINT_HEIGHT;

        // Derive the expected top from the same Fill/Length/Fill split
        // `connection_setup_layout` uses internally, rather than a hand-rolled `/ 2`: with an
        // odd amount of leftover space, ratatui's `Fill` constraints don't necessarily split
        // it evenly in floor's favor (verified empirically: they give the extra row to the
        // first `Fill`, not the second), so re-deriving it here is both correct and robust to
        // that implementation detail rather than assuming a particular rounding direction.
        let [_, expected_content, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(content_height),
            Constraint::Fill(1),
        ])
        .areas(outer);

        assert_eq!(layout.csms_url.y, expected_content.y);
    }

    #[test]
    fn does_not_panic_on_a_tiny_area() {
        let layout = connection_setup_layout(area(10, 2));
        assert!(layout.hint.bottom() <= 2);
    }
}
