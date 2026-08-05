use std::fmt;
use std::time::Duration;

use super::config::ChargerConfig;

/// How a charger's `connection_status` gets driven, and therefore who is allowed to write it.
///
/// The two variants own `ChargerState::connection_status` for mutually exclusive reasons:
/// - `Local`: there's no real CSMS in the picture, so [`ChargerState::tick`] drives the whole
///   boot-to-connected lifecycle itself, purely from simulated elapsed time.
/// - `LiveCsms`: a real OCPP connection exists, and [`super::ocpp_bridge::apply_ocpp_state`]
///   mirrors the CSMS's actual registration/connector state onto `connection_status` every time
///   a snapshot arrives. `tick` must never touch `connection_status` in this mode - doing so
///   would race the real protocol state and make the display flicker between what the bridge
///   just set and what a local simulated clock thinks it should be.
#[derive(Debug, Clone, PartialEq)]
pub enum SimulationMode {
    /// No CSMS connection: the simulator owns the whole lifecycle itself.
    Local,
    /// Driven by a live CSMS connection; the OCPP bridge owns connection_status.
    LiveCsms { url: String },
}

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
    /// State of charge as a percentage (0.0-100.0), or `None` when unknown (e.g. a vehicle
    /// synthesized from a live CSMS snapshot before any SoC has been reported - see
    /// [`super::ocpp_bridge::apply_ocpp_state`]).
    ///
    /// Deliberately an exact `f64`, not a whole-percent `u8`: [`EvseState::tick`] accumulates
    /// many small simulated increments here (the TUI ticks roughly every ~100ms), and a `u8`
    /// that gets rounded and written back on every single tick would throw away the sub-percent
    /// remainder each time - at a slow enough per-tick rate that can round every increment back
    /// down to zero and leave the value stuck forever, however long simulated time runs (see the
    /// `EvseState::tick` regression tests comparing many small ticks against one large one).
    /// Round to a whole percent only where something displays it.
    pub state_of_charge: Option<f64>,
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
    /// Simulated time this connector's current charging session has been running, accumulated by
    /// [`EvseState::tick`] from the same simulated `elapsed` used for metrics - never wall-clock.
    ///
    /// It advances only while `status` is `Charging`, holds steady (rather than resetting) across
    /// a pause such as `Charging -> Occupied -> Charging` (e.g. a cleared fault, or a CSMS
    /// suspending and resuming the same session), and resets to zero once `status` returns to
    /// `Available` - the point at which the vehicle is gone and the session is genuinely over.
    pub session_duration: Duration,
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

    /// Simulated state-of-charge gain per second of simulated time while a connector is
    /// `Charging`. This is a demo pace, not a physically derived one (it isn't back-calculated
    /// from [`Self::SIMULATED_CHARGING_POWER_KW`] and a battery capacity) - much like
    /// [`ChargerState::SIMULATED_BOOT_DURATION`] isn't a real boot time. At this rate, a vehicle
    /// plugged in at the default 20% (see [`crate::charger::Command::PlugInVehicle`]) reaches
    /// 100% after exactly 4 simulated minutes of charging, which is fast enough that a session
    /// visibly progresses within a short live demo without looking instantaneous.
    const SOC_PERCENT_PER_SECOND: f64 = 1.0 / 3.0;

    /// Advances this EVSE's simulated meter reading by `elapsed`: power and current reflect
    /// how many connectors are currently `Charging` (multiple charging connectors on one EVSE
    /// simply add up - this simulator has no per-connector meter, only an EVSE-level one), and
    /// energy accumulates accordingly. Power/current drop to zero (energy holds) once nothing's
    /// charging.
    ///
    /// Also drives two pieces of per-connector state from the same simulated `elapsed`: each
    /// `Charging` connector's [`ConnectorState::session_duration`] accumulates and its plugged-in
    /// vehicle's [`Vehicle::state_of_charge`] rises (clamped at 100, see
    /// [`Self::SOC_PERCENT_PER_SECOND`]); a connector that returns to `Available` has its session
    /// duration reset to zero since the vehicle is gone. Neither field moves for any other
    /// status (`Occupied`, `Faulted`, `Unavailable`, `Reserved`) - a connector merely paused
    /// mid-session (e.g. `Charging -> Occupied -> Charging` after a fault clears) holds its
    /// accumulated duration and SoC rather than losing them, since it's still the same session.
    ///
    /// `state_of_charge` accumulates as the exact `f64` it's stored as (see the doc comment on
    /// [`Vehicle::state_of_charge`]) - it is never rounded to a coarser type mid-simulation, only
    /// when something displays it, so the result is identical (within floating-point rounding)
    /// however many ticks the same total elapsed time is split across.
    ///
    /// Charging never tapers or stops once the vehicle reaches 100% SoC - that's
    /// charging-strategy behavior for a later phase, not this simulator's meter/session tick.
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

        for connector in &mut self.connectors {
            match connector.status {
                ConnectorStatus::Charging => {
                    connector.session_duration += elapsed;
                    if let Some(vehicle) = connector.vehicle.as_mut()
                        && let Some(soc) = vehicle.state_of_charge
                    {
                        let increase = Self::SOC_PERCENT_PER_SECOND * elapsed.as_secs_f64();
                        vehicle.state_of_charge = Some((soc + increase).min(100.0));
                    }
                }
                ConnectorStatus::Available => {
                    connector.session_duration = Duration::ZERO;
                }
                _ => {}
            }
        }
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
    /// Who drives `connection_status` - see [`SimulationMode`] for the ownership split.
    pub mode: SimulationMode,
    /// Simulated elapsed time since this charger was created, accumulated by [`Self::tick`].
    /// This is simulated time, not wall-clock, so it stays deterministic and testable.
    pub uptime: Duration,
}

impl ChargerState {
    /// Simulated time a locally-driven charger spends `Booting` before `tick` promotes it to
    /// `Connected`. ~1.5s reads as a plausible boot handshake without making a demo wait for it.
    const SIMULATED_BOOT_DURATION: Duration = Duration::from_millis(1500);

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
                        session_duration: Duration::ZERO,
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
            mode: SimulationMode::Local,
            uptime: Duration::ZERO,
        }
    }

    /// Advances every EVSE's simulated meter reading by `elapsed` (see [`EvseState::tick`]) and
    /// accumulates simulated `uptime`. In [`SimulationMode::Local`], also drives the charger from
    /// `Booting` to `Connected` once `uptime` reaches [`Self::SIMULATED_BOOT_DURATION`]. In
    /// [`SimulationMode::LiveCsms`], `connection_status` is never touched here - the OCPP bridge
    /// owns it exclusively (see [`SimulationMode`]).
    pub fn tick(&mut self, elapsed: std::time::Duration) {
        self.uptime += elapsed;

        if self.mode == SimulationMode::Local
            && self.connection_status == ConnectionStatus::Booting
            && self.uptime >= Self::SIMULATED_BOOT_DURATION
        {
            self.connection_status = ConnectionStatus::Connected;
        }

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

    #[test]
    fn a_fresh_charger_defaults_to_local_mode_with_zero_uptime() {
        let state = ChargerState::from_config(config(vec![]));
        assert_eq!(state.mode, SimulationMode::Local);
        assert_eq!(state.uptime, Duration::ZERO);
    }

    #[test]
    fn uptime_accumulates_across_ticks() {
        let mut state = ChargerState::from_config(config(vec![]));

        state.tick(Duration::from_millis(400));
        state.tick(Duration::from_millis(400));

        assert_eq!(state.uptime, Duration::from_millis(800));
    }

    #[test]
    fn a_local_charger_stays_booting_before_the_boot_duration_elapses() {
        let mut state = ChargerState::from_config(config(vec![]));

        state.tick(ChargerState::SIMULATED_BOOT_DURATION - Duration::from_millis(1));

        assert_eq!(state.connection_status, ConnectionStatus::Booting);
    }

    #[test]
    fn a_local_charger_connects_once_the_boot_duration_elapses() {
        let mut state = ChargerState::from_config(config(vec![]));

        state.tick(ChargerState::SIMULATED_BOOT_DURATION);

        assert_eq!(state.connection_status, ConnectionStatus::Connected);
    }

    #[test]
    fn the_boot_transition_survives_being_reached_across_several_small_ticks() {
        let mut state = ChargerState::from_config(config(vec![]));
        let step = ChargerState::SIMULATED_BOOT_DURATION / 10;

        for _ in 0..9 {
            state.tick(step);
        }
        assert_eq!(state.connection_status, ConnectionStatus::Booting);

        state.tick(step);

        assert_eq!(state.connection_status, ConnectionStatus::Connected);
    }

    #[test]
    fn a_local_charger_does_not_regress_from_connected_back_to_booting() {
        let mut state = ChargerState::from_config(config(vec![]));
        state.tick(ChargerState::SIMULATED_BOOT_DURATION);
        assert_eq!(state.connection_status, ConnectionStatus::Connected);

        state.tick(Duration::from_secs(3600));

        assert_eq!(state.connection_status, ConnectionStatus::Connected);
    }

    #[test]
    fn a_live_csms_charger_never_self_transitions_no_matter_how_long_it_ticks() {
        let mut state = ChargerState::from_config(config(vec![]));
        state.mode = SimulationMode::LiveCsms {
            url: "ws://csms.example/CP001".into(),
        };

        state.tick(Duration::from_secs(3600));

        assert_eq!(state.connection_status, ConnectionStatus::Booting);
        assert_eq!(state.uptime, Duration::from_secs(3600));
    }

    #[test]
    fn metrics_still_tick_normally_alongside_the_boot_lifecycle() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;

        state.tick(ChargerState::SIMULATED_BOOT_DURATION);

        assert_eq!(state.connection_status, ConnectionStatus::Connected);
        assert!(state.evses[0].metrics.energy_kwh > 0.0);
    }

    #[test]
    fn a_fresh_connector_has_zero_session_duration() {
        let state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        assert_eq!(state.evses[0].connectors[0].session_duration, Duration::ZERO);
    }

    #[test]
    fn session_duration_accumulates_while_charging() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;

        state.tick(Duration::from_secs(30));
        state.tick(Duration::from_secs(30));

        assert_eq!(state.evses[0].connectors[0].session_duration, Duration::from_secs(60));
    }

    #[test]
    fn session_duration_does_not_advance_for_a_non_charging_connector() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Occupied;

        state.tick(Duration::from_secs(60));

        assert_eq!(state.evses[0].connectors[0].session_duration, Duration::ZERO);
    }

    #[test]
    fn session_duration_holds_steady_rather_than_resetting_across_a_pause() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.tick(Duration::from_secs(60));

        // A fault interrupts the session without unplugging the vehicle.
        state.evses[0].connectors[0].status = ConnectorStatus::Occupied;
        state.tick(Duration::from_secs(60));
        assert_eq!(state.evses[0].connectors[0].session_duration, Duration::from_secs(60));

        // Clearing the fault resumes the same session rather than starting a new one.
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.tick(Duration::from_secs(30));

        assert_eq!(state.evses[0].connectors[0].session_duration, Duration::from_secs(90));
    }

    #[test]
    fn session_duration_resets_once_the_connector_returns_to_available() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.tick(Duration::from_secs(60));
        assert_eq!(state.evses[0].connectors[0].session_duration, Duration::from_secs(60));

        state.evses[0].connectors[0].status = ConnectorStatus::Available;
        state.tick(Duration::from_secs(1));

        assert_eq!(state.evses[0].connectors[0].session_duration, Duration::ZERO);
    }

    #[test]
    fn state_of_charge_rises_while_charging() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });

        // 3 simulated minutes at SOC_PERCENT_PER_SECOND (1/3 %/s) is 60 percentage points.
        state.tick(Duration::from_secs(180));

        let soc = state.evses[0].connectors[0].vehicle.as_ref().unwrap().state_of_charge.unwrap();
        assert!((soc - 80.0).abs() < 1e-9, "expected ~80.0, got {soc}");
    }

    #[test]
    fn state_of_charge_clamps_at_100_and_keeps_charging() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(95.0),
        });

        state.tick(Duration::from_secs(3600));

        let connector = &state.evses[0].connectors[0];
        assert_eq!(connector.vehicle.as_ref().unwrap().state_of_charge, Some(100.0));
        // Reaching 100% does not taper or stop the simulated charge - see the doc comment on
        // `EvseState::tick` - so metrics keep reflecting a charging connector.
        assert_eq!(state.evses[0].metrics.power_kw, EvseState::SIMULATED_CHARGING_POWER_KW);
    }

    #[test]
    fn state_of_charge_does_not_advance_while_not_charging() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Occupied;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });

        state.tick(Duration::from_secs(3600));

        assert_eq!(
            state.evses[0].connectors[0].vehicle.as_ref().unwrap().state_of_charge,
            Some(20.0)
        );
    }

    #[test]
    fn state_of_charge_stays_none_when_unknown() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: None,
        });

        state.tick(Duration::from_secs(60));

        assert_eq!(
            state.evses[0].connectors[0].vehicle.as_ref().unwrap().state_of_charge,
            None
        );
    }

    // Regression test for a real bug: `state_of_charge` used to be an `Option<u8>` that got
    // rounded and written back on every single tick. At the TUI's real ~100ms tick cadence, one
    // tick's increase (1/3 %/s * 0.1s = 0.0333...%) rounds straight back down to nothing, so the
    // value never moved no matter how long the session ran. This drives the same total simulated
    // time through many small ticks - the shape the real app actually produces - rather than one
    // big one, which is the only shape the old, buggy tests exercised.
    #[test]
    fn state_of_charge_advances_correctly_across_many_small_ticks_like_the_real_app_does() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });

        for _ in 0..600 {
            state.tick(Duration::from_millis(100));
        }

        let soc = state.evses[0].connectors[0].vehicle.as_ref().unwrap().state_of_charge.unwrap();
        assert!(
            soc > 25.0,
            "600 ticks of 100ms (60s simulated) should meaningfully raise SoC above 20%, got {soc}"
        );
    }

    #[test]
    fn state_of_charge_is_independent_of_how_the_same_total_elapsed_time_is_split_into_ticks() {
        let mut many_small_ticks = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        many_small_ticks.evses[0].connectors[0].status = ConnectorStatus::Charging;
        many_small_ticks.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });
        for _ in 0..600 {
            many_small_ticks.tick(Duration::from_millis(100));
        }

        let mut one_big_tick = ChargerState::from_config(config(vec![EvseConfig { id: 1, connectors: 1 }]));
        one_big_tick.evses[0].connectors[0].status = ConnectorStatus::Charging;
        one_big_tick.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });
        one_big_tick.tick(Duration::from_secs(60));

        let small = many_small_ticks.evses[0].connectors[0].vehicle.as_ref().unwrap().state_of_charge.unwrap();
        let big = one_big_tick.evses[0].connectors[0].vehicle.as_ref().unwrap().state_of_charge.unwrap();
        assert!(
            (small - big).abs() < 1e-6,
            "600x100ms ticks ({small}) should match one 60s tick ({big}) covering the same simulated time"
        );
    }
}
