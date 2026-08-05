use ratatui::layout::{Constraint, Layout, Rect};

/// The three input rows of the connection setup screen, plus a hint footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionSetupLayout {
    pub csms_url: Rect,
    pub ocpp_identity: Rect,
    pub password: Rect,
    pub hint: Rect,
}

const FIELD_HEIGHT: u16 = 3;
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
        let layout = connection_setup_layout(area(80, 40));
        let content_height = FIELD_HEIGHT * 3 + HINT_HEIGHT;
        let expected_top = (40 - content_height) / 2;

        assert_eq!(layout.csms_url.y, expected_top);
    }

    #[test]
    fn does_not_panic_on_a_tiny_area() {
        let layout = connection_setup_layout(area(10, 2));
        assert!(layout.hint.bottom() <= 2);
    }
}
