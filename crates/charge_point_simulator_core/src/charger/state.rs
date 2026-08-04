use std::fmt;

use super::config::ChargerConfig;

/// The charger's connection to the CSMS. Every charger starts out `Booting`
/// until the simulated boot notification flow completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Booting,
    Connected,
    Reconnecting,
    Offline,
}

impl fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            ConnectionStatus::Booting => "booting",
            ConnectionStatus::Connected => "connected",
            ConnectionStatus::Reconnecting => "reconnecting",
            ConnectionStatus::Offline => "offline",
        };
        write!(f, "{label}")
    }
}

/// The status of a single connector, loosely modeled on OCPP connector statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorStatus {
    Available,
    Occupied,
    Charging,
    Faulted,
    Unavailable,
    Reserved,
}

impl fmt::Display for ConnectorStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            ConnectorStatus::Available => "available",
            ConnectorStatus::Occupied => "occupied",
            ConnectorStatus::Charging => "charging",
            ConnectorStatus::Faulted => "faulted",
            ConnectorStatus::Unavailable => "unavailable",
            ConnectorStatus::Reserved => "reserved",
        };
        write!(f, "{label}")
    }
}

/// A mock vehicle plugged into a connector.
#[derive(Debug, Clone, PartialEq)]
pub struct Vehicle {
    pub id: String,
    pub state_of_charge: Option<u8>,
}

/// Live metering data for an EVSE.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EvseMetrics {
    pub power_kw: f64,
    pub current_a: f64,
    pub energy_kwh: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectorState {
    pub id: u32,
    pub status: ConnectorStatus,
    pub vehicle: Option<Vehicle>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvseState {
    pub id: u32,
    pub connectors: Vec<ConnectorState>,
    pub metrics: EvseMetrics,
}

/// The live, mutable state of a running charger, seeded from its
/// [`ChargerConfig`]. This is what the TUI (and later, other frontends)
/// render and what simulated events mutate.
#[derive(Debug, Clone, PartialEq)]
pub struct ChargerState {
    pub config: ChargerConfig,
    pub connection_status: ConnectionStatus,
    pub evses: Vec<EvseState>,
}

impl ChargerState {
    /// Builds the initial state for a freshly started charger: booting,
    /// every connector available, no vehicles plugged in, and zeroed metrics.
    pub fn from_config(config: ChargerConfig) -> Self {
        let evses = config
            .evses
            .iter()
            .map(|evse_config| EvseState {
                id: evse_config.id,
                connectors: (1..=evse_config.connectors)
                    .map(|connector_id| ConnectorState {
                        id: connector_id,
                        status: ConnectorStatus::Available,
                        vehicle: None,
                    })
                    .collect(),
                metrics: EvseMetrics::default(),
            })
            .collect();

        Self {
            connection_status: ConnectionStatus::Booting,
            evses,
            config,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::{EvseConfig, OcppVersion};

    fn config(evses: Vec<EvseConfig>) -> ChargerConfig {
        ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V16J,
            evses,
        }
    }

    #[test]
    fn a_fresh_charger_starts_booting() {
        let state = ChargerState::from_config(config(vec![]));
        assert_eq!(state.connection_status, ConnectionStatus::Booting);
    }

    #[test]
    fn builds_one_evse_state_per_configured_evse() {
        let state = ChargerState::from_config(config(vec![
            EvseConfig {
                id: 1,
                connectors: 2,
            },
            EvseConfig {
                id: 2,
                connectors: 1,
            },
        ]));

        assert_eq!(state.evses.len(), 2);
        assert_eq!(state.evses[0].id, 1);
        assert_eq!(state.evses[1].id, 2);
    }

    #[test]
    fn builds_one_connector_state_per_configured_connector_count() {
        let state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 3,
        }]));

        let connectors = &state.evses[0].connectors;
        assert_eq!(connectors.len(), 3);
        assert_eq!(
            connectors.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn every_connector_starts_available_with_no_vehicle() {
        let state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));

        let connector = &state.evses[0].connectors[0];
        assert_eq!(connector.status, ConnectorStatus::Available);
        assert_eq!(connector.vehicle, None);
    }

    #[test]
    fn every_evse_starts_with_zeroed_metrics() {
        let state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));

        assert_eq!(state.evses[0].metrics, EvseMetrics::default());
    }

    #[test]
    fn a_charger_with_no_evses_configured_has_no_evse_state() {
        let state = ChargerState::from_config(config(vec![]));
        assert_eq!(state.evses, Vec::new());
    }

    #[test]
    fn connection_and_connector_statuses_render_a_readable_label() {
        assert_eq!(ConnectionStatus::Connected.to_string(), "connected");
        assert_eq!(ConnectionStatus::Reconnecting.to_string(), "reconnecting");
        assert_eq!(ConnectorStatus::Charging.to_string(), "charging");
        assert_eq!(ConnectorStatus::Faulted.to_string(), "faulted");
    }
}
