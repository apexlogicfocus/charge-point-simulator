use charge_point_simulator_core::charger::{ConnectionStatus, ConnectorStatus};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Block;

/// Flowion brand teal, used as the base color for the banner and block borders.
pub const BRAND_TEAL: Color = Color::Rgb(0x14, 0xB8, 0xA6);

/// A bordered block styled with the brand teal border color.
pub fn bordered_block<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::bordered().title(title).border_style(Style::new().fg(BRAND_TEAL))
}

pub fn connection_status_color(status: ConnectionStatus) -> Color {
    match status {
        ConnectionStatus::Booting => Color::Yellow,
        ConnectionStatus::Connected => Color::Green,
        ConnectionStatus::Reconnecting => Color::Yellow,
        ConnectionStatus::Offline => Color::Red,
    }
}

pub fn connector_status_color(status: ConnectorStatus) -> Color {
    match status {
        ConnectorStatus::Available => Color::Green,
        ConnectorStatus::Occupied => Color::Yellow,
        ConnectorStatus::Charging => Color::Cyan,
        ConnectorStatus::Faulted => Color::Red,
        ConnectorStatus::Unavailable => Color::DarkGray,
        ConnectorStatus::Reserved => Color::Magenta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_connection_states_read_as_green_or_cautionary_yellow() {
        assert_eq!(
            connection_status_color(ConnectionStatus::Connected),
            Color::Green
        );
        assert_eq!(
            connection_status_color(ConnectionStatus::Booting),
            Color::Yellow
        );
        assert_eq!(
            connection_status_color(ConnectionStatus::Reconnecting),
            Color::Yellow
        );
    }

    #[test]
    fn offline_connection_reads_as_red() {
        assert_eq!(
            connection_status_color(ConnectionStatus::Offline),
            Color::Red
        );
    }

    #[test]
    fn faulted_connector_reads_as_red_and_available_as_green() {
        assert_eq!(
            connector_status_color(ConnectorStatus::Faulted),
            Color::Red
        );
        assert_eq!(
            connector_status_color(ConnectorStatus::Available),
            Color::Green
        );
    }

    #[test]
    fn every_connector_status_maps_to_a_distinct_color() {
        let statuses = [
            ConnectorStatus::Available,
            ConnectorStatus::Occupied,
            ConnectorStatus::Charging,
            ConnectorStatus::Faulted,
            ConnectorStatus::Unavailable,
            ConnectorStatus::Reserved,
        ];
        let colors: Vec<Color> = statuses.iter().copied().map(connector_status_color).collect();
        let mut unique = colors.clone();
        unique.sort_by_key(|c| format!("{c:?}"));
        unique.dedup();
        assert_eq!(unique.len(), colors.len());
    }
}
