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

impl EvseState {
    /// Simulated charging power per actively-charging connector, in kW - a plausible
    /// single-phase AC rate, not derived from any real hardware spec.
    const SIMULATED_CHARGING_POWER_KW: f64 = 7.4;
    /// Nominal single-phase voltage used to derive a simulated current reading from power.
    const NOMINAL_VOLTAGE: f64 = 230.0;

    /// Advances this EVSE's simulated meter reading by `elapsed`: power and current reflect
    /// how many connectors are currently `Charging` (multiple charging connectors on one EVSE
    /// simply add up - this simulator has no per-connector meter, only an EVSE-level one), and
    /// energy accumulates accordingly. Power/current drop to zero (energy holds) once nothing's
    /// charging.
    pub fn tick(&mut self, elapsed: std::time::Duration) {
        let charging_connectors = self
            .connectors
            .iter()
            .filter(|connector| connector.status == ConnectorStatus::Charging)
            .count();
        let power_kw = Self::SIMULATED_CHARGING_POWER_KW * charging_connectors as f64;

        self.metrics.power_kw = power_kw;
        self.metrics.current_a = if power_kw > 0.0 {
            power_kw * 1000.0 / Self::NOMINAL_VOLTAGE
        } else {
            0.0
        };
        self.metrics.energy_kwh += power_kw * (elapsed.as_secs_f64() / 3600.0);
    }
}

/// The live, mutable state of a running charger, seeded from its
/// [`ChargerConfig`]. This is what the TUI (and later, other frontends)
/// render and what simulated events mutate.
#[derive(Debug, Clone, PartialEq)]
pub struct ChargerState {
    pub config: ChargerConfig,
    pub connection_status: ConnectionStatus,
    pub evses: Vec<EvseState>,
    /// The message currently shown on the charger's display (`None` if it's blank), only
    /// meaningful when `config.has_display` is `true`. Set/cleared via
    /// [`crate::charger::Command::SetDisplayMessage`]/[`crate::charger::Command::ClearDisplayMessage`].
    pub display_message: Option<String>,
}

impl ChargerState {
    /// Builds the initial state for a freshly started charger: booting,
    /// every connector available, no vehicles plugged in, and zeroed metrics.
    pub fn from_config(config: ChargerConfig) -> Self {
        let evses: Vec<EvseState> = config
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

        tracing::info!(
            charger = %config.id,
            evses = evses.len(),
            "charger state initialized"
        );

        Self {
            connection_status: ConnectionStatus::Booting,
            evses,
            config,
            display_message: None,
        }
    }

    /// Advances every EVSE's simulated meter reading by `elapsed` (see [`EvseState::tick`]).
    pub fn tick(&mut self, elapsed: std::time::Duration) {
        for evse in &mut self.evses {
            evse.tick(elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::{EvseConfig, OcppVersion};
    use std::time::Duration;

    fn config(evses: Vec<EvseConfig>) -> ChargerConfig {
        ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V16J,
            evses,
            has_display: false,
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
    fn a_fresh_charger_has_no_display_message() {
        let state = ChargerState::from_config(config(vec![]));
        assert_eq!(state.display_message, None);
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
    fn ticking_with_no_charging_connectors_leaves_metrics_at_zero() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));

        state.tick(Duration::from_secs(3600));

        assert_eq!(state.evses[0].metrics, EvseMetrics::default());
    }

    #[test]
    fn ticking_an_hour_with_one_charging_connector_adds_a_full_hour_of_energy() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;

        state.tick(Duration::from_secs(3600));

        let metrics = state.evses[0].metrics;
        assert_eq!(metrics.power_kw, EvseState::SIMULATED_CHARGING_POWER_KW);
        assert!((metrics.energy_kwh - EvseState::SIMULATED_CHARGING_POWER_KW).abs() < 1e-9);
        assert!(metrics.current_a > 0.0);
    }

    #[test]
    fn energy_accumulates_across_multiple_ticks() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;

        state.tick(Duration::from_secs(1800));
        state.tick(Duration::from_secs(1800));

        assert!((state.evses[0].metrics.energy_kwh - EvseState::SIMULATED_CHARGING_POWER_KW).abs() < 1e-9);
    }

    #[test]
    fn two_charging_connectors_on_one_evse_double_the_simulated_power() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 2 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.evses[0].connectors[1].status = ConnectorStatus::Charging;

        state.tick(Duration::from_secs(3600));

        assert_eq!(
            state.evses[0].metrics.power_kw,
            EvseState::SIMULATED_CHARGING_POWER_KW * 2.0
        );
    }

    #[test]
    fn power_and_current_drop_back_to_zero_once_charging_stops_but_energy_holds() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.tick(Duration::from_secs(3600));
        let energy_after_charging = state.evses[0].metrics.energy_kwh;

        state.evses[0].connectors[0].status = ConnectorStatus::Available;
        state.tick(Duration::from_secs(3600));

        let metrics = state.evses[0].metrics;
        assert_eq!(metrics.power_kw, 0.0);
        assert_eq!(metrics.current_a, 0.0);
        assert_eq!(metrics.energy_kwh, energy_after_charging);
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
