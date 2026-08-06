use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::App;
use crate::text_field::TextField;
use crate::theme;

/// The card enclosing the whole screen, and the input rows/hint/error/suggestions within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionSetupLayout {
    /// The titled card's own outer bounds - naming the charger being connected is what makes
    /// this a "card" rather than just three fields floating in the middle of the screen.
    pub card: Rect,
    pub csms_url: Rect,
    /// Always allotted, like the parameter prompt's error row (see `palette.rs`), so the field
    /// below it doesn't jump the moment a bad URL is rejected.
    pub csms_url_error: Rect,
    pub csms_url_suggestions: Rect,
    pub ocpp_identity: Rect,
    pub password: Rect,
    pub hint: Rect,
}

// 2, not 3: each field is now a `theme::section` (1 row of chrome, the top rule) rather than
// a 4-sided `bordered_block` (2 rows of chrome), so it needs 1 fewer row to show the same
// single content line.
const FIELD_HEIGHT: u16 = 2;
const ERROR_HEIGHT: u16 = 1;
const SUGGESTIONS_HEIGHT: u16 = 1;
// 2, not 1: the hint line has grown too long for one row at `CARD_WIDTH` since Phase 6 added
// the reveal/suggestion bindings to it, so it wraps - see the `Wrap` on its `Paragraph` in
// `render`. Reserving a fixed 2 rows (rather than measuring the wrapped text) keeps the layout
// static-only, at the cost of a trailing blank row whenever the text happens to fit on one.
const HINT_HEIGHT: u16 = 2;
/// Wide enough for a realistic `wss://` URL plus its label without wrapping, but capped rather
/// than filling a wide terminal - a form this narrow reads better centered than stretched edge
/// to edge.
const CARD_WIDTH: u16 = 70;

/// Lays out the titled card (see [`ConnectionSetupLayout::card`]) centered in `area`, and the
/// input rows/error/suggestions/hint stacked top to bottom within it.
pub fn connection_setup_layout(area: Rect) -> ConnectionSetupLayout {
    let content_height = FIELD_HEIGHT * 3 + ERROR_HEIGHT + SUGGESTIONS_HEIGHT + HINT_HEIGHT;
    // +2: the card's own top/bottom border, on top of the content it encloses.
    let card = super::centered_rect(CARD_WIDTH, content_height + 2, area);
    let inner = Block::bordered().inner(card);

    let [
        csms_url,
        csms_url_error,
        csms_url_suggestions,
        ocpp_identity,
        password,
        hint,
    ] = Layout::vertical([
        Constraint::Length(FIELD_HEIGHT),
        Constraint::Length(ERROR_HEIGHT),
        Constraint::Length(SUGGESTIONS_HEIGHT),
        Constraint::Length(FIELD_HEIGHT),
        Constraint::Length(FIELD_HEIGHT),
        Constraint::Length(HINT_HEIGHT),
    ])
    .areas(inner);

    ConnectionSetupLayout {
        card,
        csms_url,
        csms_url_error,
        csms_url_suggestions,
        ocpp_identity,
        password,
        hint,
    }
}

pub(super) fn render(frame: &mut Frame, app: &App) {
    let layout = connection_setup_layout(frame.area());

    let charger_id = app
        .charger_state
        .as_ref()
        .map(|state| state.config.id.as_str())
        .unwrap_or("charger");
    frame.render_widget(
        theme::bordered_block(format!("Connect {charger_id} to a CSMS")),
        layout.card,
    );

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
        let display_value = if title == "Password" && !app.connection_password_revealed {
            "*".repeat(field.value().chars().count())
        } else {
            field.value().to_string()
        };
        // Unlike the dashboard's panels, this screen already tracks genuine per-field focus
        // (`connection_focused_field`), so `focused` here is real, not a placeholder - the
        // focused field's section rule lights up in `chrome_focused()`'s brand teal.
        frame.render_widget(
            Paragraph::new(Line::styled(display_value, theme::text()))
                .block(theme::section(title, focused)),
            area,
        );
        if focused {
            frame.set_cursor_position((area.x + field.cursor() as u16, area.y + 1));
        }
    }

    if let Some(error) = app.connection_url_error {
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {error}"), theme::error())),
            layout.csms_url_error,
        );
    }

    let suggestions = app.connection_url_suggestions();
    if !suggestions.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" recent: {}", suggestions.join("  ")),
                theme::text_muted(),
            )),
            layout.csms_url_suggestions,
        );
    }

    frame.render_widget(
        Paragraph::new(Line::styled(
            "Tab: next field  Ctrl+R: reveal password  PgUp/PgDn: recent URL  \
             Enter: connect  Esc: cancel",
            theme::text_dim(),
        ))
        .wrap(Wrap { trim: false }),
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

        assert_eq!(layout.csms_url_error.y, layout.csms_url.bottom());
        assert_eq!(
            layout.csms_url_suggestions.y,
            layout.csms_url_error.bottom()
        );
        assert_eq!(layout.ocpp_identity.y, layout.csms_url_suggestions.bottom());
        assert_eq!(layout.password.y, layout.ocpp_identity.bottom());
        assert_eq!(layout.hint.y, layout.password.bottom());
    }

    #[test]
    fn fields_use_the_configured_height() {
        let layout = connection_setup_layout(area(80, 30));

        assert_eq!(layout.csms_url.height, FIELD_HEIGHT);
        assert_eq!(layout.csms_url_error.height, ERROR_HEIGHT);
        assert_eq!(layout.csms_url_suggestions.height, SUGGESTIONS_HEIGHT);
        assert_eq!(layout.ocpp_identity.height, FIELD_HEIGHT);
        assert_eq!(layout.password.height, FIELD_HEIGHT);
        assert_eq!(layout.hint.height, HINT_HEIGHT);
    }

    #[test]
    fn the_card_names_no_specific_charger_here_but_is_centered_when_space_allows() {
        let outer = area(80, 40);
        let layout = connection_setup_layout(outer);
        let content_height = FIELD_HEIGHT * 3 + ERROR_HEIGHT + SUGGESTIONS_HEIGHT + HINT_HEIGHT;

        // Re-derive the expected card position from `centered_rect` itself rather than a
        // hand-rolled midpoint - this is exactly the calculation `connection_setup_layout`
        // delegates to, so re-deriving it here stays correct regardless of that function's
        // internal rounding behavior.
        let expected_card = crate::ui::centered_rect(CARD_WIDTH, content_height + 2, outer);

        assert_eq!(layout.card, expected_card);
        assert_eq!(layout.csms_url.y, expected_card.y + 1);
    }

    #[test]
    fn does_not_panic_on_a_tiny_area() {
        let layout = connection_setup_layout(area(10, 2));
        assert!(layout.hint.bottom() <= 2);
    }
}
