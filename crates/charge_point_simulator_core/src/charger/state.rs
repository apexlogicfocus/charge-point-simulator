use std::fmt;
use std::time::Duration;

use super::config::ChargerConfig;

/// How a charger's `connection_status` gets driven.
///
/// As of `docs/hardware-roadmap.md`'s H3b, both variants run a real
/// `ocpp_charge_point::ChargePointRuntime` (see [`crate::charger::RunningCharger`]) and
/// `connection_status` comes exclusively from [`super::ocpp_bridge::apply_ocpp_state`], which
/// reads this to decide *how* to interpret the runtime's `ChargePointState` - never from
/// [`ChargerState::tick`], which no longer touches `connection_status` at all:
/// - `Local`: there is no CSMS to register with (`RunningCharger::register`/
///   `register_until_accepted` are simply never called), so `ChargePointState::registration`
///   stays `None` forever. `apply_ocpp_state` reads that, via this variant, as
///   [`ConnectionStatus::Offline`] rather than the endlessly-retried `Booting` an unanswered
///   real CSMS registration would mean.
/// - `LiveCsms`: a real OCPP connection exists, and `apply_ocpp_state` mirrors the CSMS's actual
///   registration status onto `connection_status` every time a snapshot arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum SimulationMode {
    /// No CSMS connection: the simulator runs its own hardware with nothing on the other end.
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
    /// Whether the connector's cable lock actuator currently reports engaged. Written exclusively
    /// by [`super::ocpp_bridge::apply_hardware_state`], which reads
    /// [`super::hardware::FakeConnector::is_locked`] off the running charger's hardware handle -
    /// unlike everything else on this struct, this has no `ChargePointState` counterpart to read
    /// instead, because the lock only exists in the hardware layer
    /// (`docs/hardware-roadmap.md`'s H7).
    pub locked: bool,
    /// Whether the connector's contactor currently reports closed (energy can flow). Written
    /// exclusively by [`super::ocpp_bridge::apply_hardware_state`], reading
    /// [`super::hardware::FakeConnector::is_contactor_closed`] - same H7 rationale as
    /// [`Self::locked`].
    pub contactor_closed: bool,
    /// The current limit last applied to this connector (e.g. by a CSMS smart charging profile),
    /// in mA, or `None` when no limit currently applies. `Some(0)` ("suspend charging") and
    /// `None` ("unlimited") are distinct and both round-trip. Written exclusively by
    /// [`super::ocpp_bridge::apply_hardware_state`], reading
    /// [`super::hardware::FakeConnector::current_limit_ma`] - same H7 rationale as
    /// [`Self::locked`].
    pub current_limit_ma: Option<u32>,
    /// Whether this connector's meter is currently running in export (V2G discharge) direction
    /// rather than import (`docs/hardware-roadmap.md`'s H14). Written exclusively by
    /// [`super::ocpp_bridge::apply_hardware_state`], reading
    /// [`super::hardware::FakeConnector::is_discharging`] - same H7 rationale as [`Self::locked`].
    ///
    /// No CSMS message can ever set this: `HardwareCommand` cannot carry direction, so discharge
    /// is reachable only through [`super::running_charger::RunningCharger::set_discharging`] (see
    /// its doc comment, and the roadmap's "Known gaps"). A frontend showing this field is
    /// therefore reporting a deliberate local action, never something the protocol did.
    pub discharging: bool,
    /// Cumulative energy this connector has exported so far, in Wh - the counterpart to
    /// [`EvseMetrics::energy_kwh`], which is OCPP's *import* register and deliberately freezes
    /// rather than running backwards while discharging (H14a). Written exclusively by
    /// [`super::ocpp_bridge::apply_hardware_state`], reading
    /// [`super::hardware::FakeConnector::exported_energy_wh`].
    ///
    /// Per connector rather than per EVSE, unlike `EvseMetrics`: `MeterSample` carries no export
    /// figure at all, so there is nothing for [`super::ocpp_bridge::apply_ocpp_state`] to sum on
    /// the EVSE's behalf and no reason to invent an aggregate the hardware never reports.
    pub exported_energy_wh: i64,
}

/// A rolling window of recent power readings for one EVSE, for anything that wants to show a trend
/// rather than an instant - a sparkline, a chart, a "was it flat for the last minute?" check.
///
/// Lives here rather than in the TUI on purpose (`docs/tui-roadmap.md`'s settled decision 2): a
/// history is data, and every consumer of the published crate can use it, while a sparkline drawn
/// from it is presentation and stays in whichever frontend draws one.
///
/// # Why it samples rather than recording every value
///
/// [`EvseState::tick`] runs at whatever cadence its caller ticks - roughly every 100ms in the TUI -
/// so recording one value per tick would hold about six seconds in a 60-slot window, and the window
/// would silently change length with the frame rate. Instead this accumulates simulated time and
/// appends one sample per [`Self::SAMPLE_INTERVAL`], so [`Self::CAPACITY`] samples always span the
/// same simulated duration ([`Self::window`]) however the caller slices its ticks - the same
/// "simulated time, never wall clock" rule the rest of this module follows, and the same
/// many-small-ticks-equal-one-big-tick property [`Vehicle::state_of_charge`] needs.
///
/// Values are `power_kw` exactly as [`EvseMetrics`] carries it, **signed**: a discharging connector
/// reads negative (H14), and flattening that to a magnitude here would throw away the one thing a
/// power trend most needs to show.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PowerHistory {
    samples: Vec<f64>,
    /// Simulated time accumulated since the last sample was appended, always less than
    /// [`Self::SAMPLE_INTERVAL`] after [`Self::record`] returns.
    pending: Duration,
}

impl PowerHistory {
    /// How much simulated time each sample covers.
    pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
    /// How many samples are kept. With [`Self::SAMPLE_INTERVAL`] at one second, one minute of
    /// history - long enough for a charging session's ramp to be visible, short enough that a change
    /// now is still visible in a minute's time.
    pub const CAPACITY: usize = 60;

    /// The simulated duration a full window covers.
    pub fn window(&self) -> Duration {
        Self::SAMPLE_INTERVAL * Self::CAPACITY as u32
    }

    /// The samples held, oldest first. Empty until the first [`Self::SAMPLE_INTERVAL`] of simulated
    /// time has been recorded - a charger that has only just started has no *history*, and
    /// synthesizing one from its current reading would invent a past it didn't have.
    pub fn samples(&self) -> &[f64] {
        &self.samples
    }

    /// Accumulates `elapsed` and appends `power_kw` once a whole [`Self::SAMPLE_INTERVAL`] has built
    /// up, dropping the oldest sample past [`Self::CAPACITY`].
    ///
    /// A tick longer than one interval appends one sample per interval it covers, all with the same
    /// value - the only honest thing available, since no reading was taken in between, and it keeps
    /// the window's span independent of tick size (a 60s tick fills the window, exactly as 600
    /// 100ms ticks would).
    pub fn record(&mut self, elapsed: Duration, power_kw: f64) {
        self.pending += elapsed;
        while self.pending >= Self::SAMPLE_INTERVAL {
            self.pending -= Self::SAMPLE_INTERVAL;
            self.samples.push(power_kw);
            if self.samples.len() > Self::CAPACITY {
                let excess = self.samples.len() - Self::CAPACITY;
                self.samples.drain(..excess);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvseState {
    pub id: u32,
    pub connectors: Vec<ConnectorState>,
    pub metrics: EvseMetrics,
    /// Recent `metrics.power_kw` readings - see [`PowerHistory`]. Filled by [`Self::tick`] from
    /// whatever `metrics` holds when it runs, which is why [`super::ocpp_bridge::apply_ocpp_state`]
    /// (the only thing that writes `metrics`) has to run first: a caller that ticked before applying
    /// the frame's snapshot would be recording the previous frame's reading.
    pub power_history: PowerHistory,
}

impl EvseState {
    /// Simulated state-of-charge gain per second of simulated time while a connector is
    /// `Charging`. This is a demo pace, not a physically derived one. At this rate, a vehicle
    /// plugged in at the default 20% (see [`crate::charger::Command::PlugInVehicle`]) reaches
    /// 100% after exactly 4 simulated minutes of charging, which is fast enough that a session
    /// visibly progresses within a short live demo without looking instantaneous.
    const SOC_PERCENT_PER_SECOND: f64 = 1.0 / 3.0;

    /// Drives two pieces of per-connector state from the simulated `elapsed`: each `Charging`
    /// connector's [`ConnectorState::session_duration`] accumulates and its plugged-in vehicle's
    /// [`Vehicle::state_of_charge`] rises (clamped at 100, see [`Self::SOC_PERCENT_PER_SECOND`]);
    /// a connector that returns to `Available` has its session duration reset to zero since the
    /// vehicle is gone. Neither field moves for any other status (`Occupied`, `Faulted`,
    /// `Unavailable`, `Reserved`) - a connector merely paused mid-session (e.g.
    /// `Charging -> Occupied -> Charging` after a fault clears) holds its accumulated duration
    /// and SoC rather than losing them, since it's still the same session.
    ///
    /// `state_of_charge` accumulates as the exact `f64` it's stored as (see the doc comment on
    /// [`Vehicle::state_of_charge`]) - it is never rounded to a coarser type mid-simulation, only
    /// when something displays it, so the result is identical (within floating-point rounding)
    /// however many ticks the same total elapsed time is split across.
    ///
    /// Charging never tapers or stops once the vehicle reaches 100% SoC - that's
    /// charging-strategy behavior for a later phase, not this simulator's session tick.
    ///
    /// It also records this EVSE's current `power_kw` into [`Self::power_history`], which is
    /// bookkeeping over a reading rather than a simulation of one - the reading itself comes from the
    /// hardware, through [`super::ocpp_bridge::apply_ocpp_state`], which must therefore have run for
    /// this frame already (see the field's own doc comment).
    ///
    /// This is deliberately the only thing left in here (`docs/hardware-roadmap.md`'s H3b): the
    /// electrical simulation (power, current, energy) that used to live alongside it moved into
    /// the hardware layer's `SimulatedMeter`, driven by `FakeChargePoint::tick` and read back
    /// through [`super::ocpp_bridge::apply_ocpp_state`] into [`Self::metrics`] - there is no
    /// vehicle/battery model down there yet, so `session_duration`/`state_of_charge` stay here
    /// until one exists.
    pub fn tick(&mut self, elapsed: std::time::Duration) {
        self.power_history.record(elapsed, self.metrics.power_kw);

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
                        locked: false,
                        contactor_closed: false,
                        current_limit_ma: None,
                        discharging: false,
                        exported_energy_wh: 0,
                    })
                    .collect(),
                metrics: EvseMetrics::default(),
                power_history: PowerHistory::default(),
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

    /// Accumulates simulated `uptime` and advances every EVSE's session/SoC bookkeeping (see
    /// [`EvseState::tick`]) by `elapsed`. Never touches `connection_status` or any EVSE's
    /// `metrics` - both are projections of a real `ChargePointState`, written exclusively by
    /// [`super::ocpp_bridge::apply_ocpp_state`], for a local charger and a live-CSMS one alike
    /// (see [`SimulationMode`] and `docs/hardware-roadmap.md`'s H3b).
    pub fn tick(&mut self, elapsed: std::time::Duration) {
        self.uptime += elapsed;

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
            capabilities: Default::default(),
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

    /// H7: lock, contactor and current limit have no hardware behind them yet at construction
    /// time, so they start unlocked/open/unlimited, exactly like a real connector that has never
    /// been actuated.
    #[test]
    fn a_fresh_connector_is_unlocked_open_and_unlimited() {
        let state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));

        let connector = &state.evses[0].connectors[0];
        assert!(!connector.locked);
        assert!(!connector.contactor_closed);
        assert_eq!(connector.current_limit_ma, None);
    }

    /// H14: a connector nothing has actuated is importing (the default direction) and has exported
    /// nothing - the same "no hardware behind it yet" starting point the H7 fields have.
    #[test]
    fn a_fresh_connector_is_importing_and_has_exported_nothing() {
        let state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));

        let connector = &state.evses[0].connectors[0];
        assert!(!connector.discharging);
        assert_eq!(connector.exported_energy_wh, 0);
    }

    // --- power history (tui-roadmap settled decision 2) -----------------------------------

    #[test]
    fn a_fresh_evse_has_no_power_history_at_all() {
        let state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));

        assert!(
            state.evses[0].power_history.samples().is_empty(),
            "a charger that just started has no past to show"
        );
    }

    #[test]
    fn power_history_samples_once_per_interval_of_simulated_time() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].metrics.power_kw = 7.4;

        // Half an interval: not a sample yet.
        state.tick(PowerHistory::SAMPLE_INTERVAL / 2);
        assert!(state.evses[0].power_history.samples().is_empty());

        // The other half completes it.
        state.tick(PowerHistory::SAMPLE_INTERVAL / 2);
        assert_eq!(state.evses[0].power_history.samples(), [7.4]);

        state.tick(PowerHistory::SAMPLE_INTERVAL);
        assert_eq!(state.evses[0].power_history.samples(), [7.4, 7.4]);
    }

    /// The property the whole sampling design exists for, and the same one SoC progression needs:
    /// how the caller slices its ticks must not change what a full window covers. Ticking at the
    /// TUI's real ~100ms cadence and ticking once must agree.
    #[test]
    fn power_history_holds_the_same_span_however_the_ticks_are_sliced() {
        let mut many_small = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        many_small.evses[0].metrics.power_kw = 7.4;
        for _ in 0..100 {
            many_small.tick(Duration::from_millis(100));
        }

        let mut one_big = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        one_big.evses[0].metrics.power_kw = 7.4;
        one_big.tick(Duration::from_secs(10));

        assert_eq!(
            many_small.evses[0].power_history.samples().len(),
            10,
            "ten seconds of simulated time is ten samples"
        );
        assert_eq!(
            many_small.evses[0].power_history.samples(),
            one_big.evses[0].power_history.samples()
        );
    }

    #[test]
    fn power_history_keeps_the_most_recent_samples_and_drops_the_rest() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));

        // Fill the window with a distinguishable value, then overwrite it with another.
        state.evses[0].metrics.power_kw = 1.0;
        state.tick(PowerHistory::SAMPLE_INTERVAL * PowerHistory::CAPACITY as u32);
        assert_eq!(
            state.evses[0].power_history.samples().len(),
            PowerHistory::CAPACITY
        );

        state.evses[0].metrics.power_kw = 2.0;
        state.tick(PowerHistory::SAMPLE_INTERVAL * 3);

        let samples = state.evses[0].power_history.samples();
        assert_eq!(
            samples.len(),
            PowerHistory::CAPACITY,
            "the window is bounded"
        );
        assert_eq!(&samples[samples.len() - 3..], [2.0, 2.0, 2.0]);
        assert_eq!(
            samples[0], 1.0,
            "the oldest three were dropped, not the newest"
        );
    }

    /// H14: a discharging EVSE reads negative, and the history has to keep the sign - a trend that
    /// showed export and import identically would be the one thing it must not do.
    #[test]
    fn power_history_keeps_the_sign_of_an_exporting_evse() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].metrics.power_kw = -7.4;

        state.tick(PowerHistory::SAMPLE_INTERVAL);

        assert_eq!(state.evses[0].power_history.samples(), [-7.4]);
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

    /// H3b: `EvseState::tick` no longer computes anything electrical - `metrics` is written
    /// exclusively by `ocpp_bridge::apply_ocpp_state`, reading real meter samples off a
    /// `ChargePointState`. See `charger/hardware/metering.rs`'s `SimulatedMeter` tests for the
    /// physics this used to duplicate, and `ocpp_bridge.rs`'s `apply_ocpp_state_populates_*`
    /// tests for where it lives now.
    #[test]
    fn ticking_never_touches_metrics_regardless_of_connector_status() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;

        state.tick(Duration::from_secs(3600));

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

    /// H3b: `tick` no longer drives any boot-to-connected lifecycle itself - `connection_status`
    /// is written exclusively by `ocpp_bridge::apply_ocpp_state`, for a local charger and a
    /// live-CSMS one alike (see `SimulationMode`'s doc comment). This holds regardless of how
    /// long a `Local` charger ticks with nothing else acting on it.
    #[test]
    fn a_local_chargers_connection_status_never_moves_from_ticking_alone() {
        let mut state = ChargerState::from_config(config(vec![]));

        state.tick(Duration::from_secs(3600));

        assert_eq!(state.connection_status, ConnectionStatus::Booting);
        assert_eq!(state.uptime, Duration::from_secs(3600));
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
    fn a_fresh_connector_has_zero_session_duration() {
        let state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        assert_eq!(
            state.evses[0].connectors[0].session_duration,
            Duration::ZERO
        );
    }

    #[test]
    fn session_duration_accumulates_while_charging() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;

        state.tick(Duration::from_secs(30));
        state.tick(Duration::from_secs(30));

        assert_eq!(
            state.evses[0].connectors[0].session_duration,
            Duration::from_secs(60)
        );
    }

    #[test]
    fn session_duration_does_not_advance_for_a_non_charging_connector() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Occupied;

        state.tick(Duration::from_secs(60));

        assert_eq!(
            state.evses[0].connectors[0].session_duration,
            Duration::ZERO
        );
    }

    #[test]
    fn session_duration_holds_steady_rather_than_resetting_across_a_pause() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.tick(Duration::from_secs(60));

        // A fault interrupts the session without unplugging the vehicle.
        state.evses[0].connectors[0].status = ConnectorStatus::Occupied;
        state.tick(Duration::from_secs(60));
        assert_eq!(
            state.evses[0].connectors[0].session_duration,
            Duration::from_secs(60)
        );

        // Clearing the fault resumes the same session rather than starting a new one.
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.tick(Duration::from_secs(30));

        assert_eq!(
            state.evses[0].connectors[0].session_duration,
            Duration::from_secs(90)
        );
    }

    #[test]
    fn session_duration_resets_once_the_connector_returns_to_available() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.tick(Duration::from_secs(60));
        assert_eq!(
            state.evses[0].connectors[0].session_duration,
            Duration::from_secs(60)
        );

        state.evses[0].connectors[0].status = ConnectorStatus::Available;
        state.tick(Duration::from_secs(1));

        assert_eq!(
            state.evses[0].connectors[0].session_duration,
            Duration::ZERO
        );
    }

    #[test]
    fn state_of_charge_rises_while_charging() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });

        // 3 simulated minutes at SOC_PERCENT_PER_SECOND (1/3 %/s) is 60 percentage points.
        state.tick(Duration::from_secs(180));

        let soc = state.evses[0].connectors[0]
            .vehicle
            .as_ref()
            .unwrap()
            .state_of_charge
            .unwrap();
        assert!((soc - 80.0).abs() < 1e-9, "expected ~80.0, got {soc}");
    }

    #[test]
    fn state_of_charge_clamps_at_100_and_keeps_charging() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(95.0),
        });

        state.tick(Duration::from_secs(3600));

        let connector = &state.evses[0].connectors[0];
        assert_eq!(
            connector.vehicle.as_ref().unwrap().state_of_charge,
            Some(100.0)
        );
        // Reaching 100% does not taper or stop the simulated charge - see the doc comment on
        // `EvseState::tick` - the connector itself stays `Charging`.
        assert_eq!(connector.status, ConnectorStatus::Charging);
    }

    #[test]
    fn state_of_charge_does_not_advance_while_not_charging() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Occupied;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });

        state.tick(Duration::from_secs(3600));

        assert_eq!(
            state.evses[0].connectors[0]
                .vehicle
                .as_ref()
                .unwrap()
                .state_of_charge,
            Some(20.0)
        );
    }

    #[test]
    fn state_of_charge_stays_none_when_unknown() {
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: None,
        });

        state.tick(Duration::from_secs(60));

        assert_eq!(
            state.evses[0].connectors[0]
                .vehicle
                .as_ref()
                .unwrap()
                .state_of_charge,
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
        let mut state = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        state.evses[0].connectors[0].status = ConnectorStatus::Charging;
        state.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });

        for _ in 0..600 {
            state.tick(Duration::from_millis(100));
        }

        let soc = state.evses[0].connectors[0]
            .vehicle
            .as_ref()
            .unwrap()
            .state_of_charge
            .unwrap();
        assert!(
            soc > 25.0,
            "600 ticks of 100ms (60s simulated) should meaningfully raise SoC above 20%, got {soc}"
        );
    }

    #[test]
    fn state_of_charge_is_independent_of_how_the_same_total_elapsed_time_is_split_into_ticks() {
        let mut many_small_ticks = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        many_small_ticks.evses[0].connectors[0].status = ConnectorStatus::Charging;
        many_small_ticks.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });
        for _ in 0..600 {
            many_small_ticks.tick(Duration::from_millis(100));
        }

        let mut one_big_tick = ChargerState::from_config(config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]));
        one_big_tick.evses[0].connectors[0].status = ConnectorStatus::Charging;
        one_big_tick.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(20.0),
        });
        one_big_tick.tick(Duration::from_secs(60));

        let small = many_small_ticks.evses[0].connectors[0]
            .vehicle
            .as_ref()
            .unwrap()
            .state_of_charge
            .unwrap();
        let big = one_big_tick.evses[0].connectors[0]
            .vehicle
            .as_ref()
            .unwrap()
            .state_of_charge
            .unwrap();
        assert!(
            (small - big).abs() < 1e-6,
            "600x100ms ticks ({small}) should match one 60s tick ({big}) covering the same simulated time"
        );
    }
}
