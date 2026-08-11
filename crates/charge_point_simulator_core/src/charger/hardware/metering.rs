use std::sync::Mutex;
use std::time::Duration;

use ocpp_charge_point::state::MeterSample;

/// Simulated charging power a connector draws while its contactor is closed and no CSMS current
/// limit clamps it further, in kW - a plausible single-phase AC rate, not derived from any real
/// hardware spec.
///
/// This module is the sole home for the constant - `charger/state.rs`'s old `EvseState`
/// accumulator, which used to duplicate it, was deleted in `docs/hardware-roadmap.md`'s H3b once
/// a local (unconnected) simulation had this hardware layer to route its meter samples through.
///
/// H14a (V2G) reuses this same magnitude for the nominal *discharge* rate rather than declaring a
/// separate constant: a connector's contactor and wiring bound the current it can carry in either
/// direction, so a real bidirectional charger's export ceiling is typically the same rating as its
/// import ceiling. Nothing in the roadmap or `capabilities:` schema asks for an asymmetric rate,
/// and inventing one unrequested would be exactly the kind of speculative surface `CLAUDE.md`'s
/// TDD discipline exists to keep out - a discharge-rate config field can be added later if a real
/// scenario needs one, the same way `current_limit_ma` already lets a CSMS clamp below it.
pub const SIMULATED_CHARGING_POWER_KW: f64 = 7.4;

/// Nominal single-phase voltage used to convert between the simulated power and current
/// readings. See [`SIMULATED_CHARGING_POWER_KW`]'s doc comment.
pub const NOMINAL_VOLTAGE: f64 = 230.0;

/// The nominal current draw at [`SIMULATED_CHARGING_POWER_KW`]/[`NOMINAL_VOLTAGE`], in
/// milliamps - the ceiling [`SimulatedMeter::tick`] clamps against when a CSMS current limit
/// applies.
fn nominal_current_ma() -> f64 {
    SIMULATED_CHARGING_POWER_KW * 1_000.0 / NOMINAL_VOLTAGE * 1_000.0
}

/// Which way power currently flows at a connector: from the grid into the vehicle (`Import`,
/// ordinary charging) or from the vehicle back to the grid (`Export`, V2G discharge).
///
/// Deliberately a `tick` argument, not a field baked into [`SimulatedMeter`] at construction: per
/// the hardware roadmap's H14 entry, "direction is a property of the session, not of a
/// connector's wiring" - a connector that *can* export doesn't always export, and which way it's
/// currently going can change mid-transaction as a CSMS-driven DER control setpoint changes. The
/// caller (`FakeConnector`, in `connector.rs`) is what actually holds that as mutable session
/// state; this type just reacts to it every tick, the same as it already does for
/// `current_limit_ma`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerDirection {
    /// Power flows from the grid into the vehicle.
    #[default]
    Import,
    /// Power flows from the vehicle back to the grid (V2G discharge).
    Export,
}

/// One connector's simulated electrical state. Accumulates in `f64` (decision 3 in the hardware
/// roadmap's "Decisions taken") and is converted to [`MeterSample`]'s integer fields only at the
/// point of emission in [`SimulatedMeter::tick`] - never inside the accumulator itself, so
/// splitting the same total `elapsed` across many small ticks or one large one gives the same
/// result within float tolerance (see the working agreement on accumulators coarser than their
/// increment).
#[derive(Debug, Default)]
struct MeterState {
    /// Cumulative *imported* energy, in Wh - OCPP's `Energy.Active.Import.Register`, the one
    /// field [`MeterSample::energy_wh`] actually reports. A register, not a running total: it
    /// only ever increases, exactly like the physical meter register it represents, so it must
    /// not move at all while [`PowerDirection::Export`] is active - see [`SimulatedMeter::tick`]'s
    /// doc comment for the reasoning.
    imported_energy_wh: f64,
    /// Cumulative *exported* energy, in Wh - the discharge counterpart to `imported_energy_wh`,
    /// kept as its own monotonically-increasing accumulator for the same reason the import side
    /// is one: conflating the two (e.g. by summing signed energy into a single field) would let a
    /// discharge session silently erase energy a CSMS was already told about, which is exactly
    /// the "getting it wrong silently corrupts every energy reading" failure this module exists to
    /// avoid.
    ///
    /// Not surfaced through [`MeterSample`] today - upstream's struct has exactly one energy
    /// field, documented as the *import* register specifically, with no
    /// `Energy.Active.Export.Register` counterpart to report this through. Tracked here anyway,
    /// reachable via [`SimulatedMeter::exported_energy_wh`], so the physics are honest (a
    /// discharge session's own accounting isn't lossy) and so a future wire-format or registration
    /// change has real numbers to expose immediately rather than another rework - the same
    /// reasoning decision 4 already applied to `power_w`'s sign.
    exported_energy_wh: f64,
}

/// A per-connector simulated meter: the physics that neither
/// [`ocpp_charge_point::hardware::Connector`] nor [`ocpp_charge_point::hardware::ChargePoint`]
/// express on their own. Advanced by an injected `elapsed` (decision 2 - no
/// `tokio::time::interval`, no wall clock, no `sleep` anywhere in this type).
///
/// The hardware layer has no reachable view of the OCPP connector state machine - that lives in
/// `charger/state.rs`, on the far side of the bridge - and, per the roadmap's guiding principles,
/// doesn't need one: whether the contactor is closed is the physically correct signal for whether
/// current is flowing, and `FakeConnector` already tracks it. See [`Self::tick`].
#[derive(Debug, Default)]
pub struct SimulatedMeter {
    state: Mutex<MeterState>,
}

impl SimulatedMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advances this connector's meter by `elapsed` and returns the resulting sample.
    ///
    /// `contactor_closed` is the sole signal for whether current flows - not any OCPP-level
    /// notion of "charging", which this layer cannot see. `current_limit_ma` clamps the
    /// *magnitude* of the nominal current draw, in either direction:
    ///
    /// - `None` means no CSMS-imposed limit - the nominal rate applies.
    /// - `Some(0)` means suspended: the clamp floors current (and so power and energy accrual) at
    ///   zero, but this is a rate of zero, not a fault - the connector is not faulted and any
    ///   transaction is not ended. This holds exactly the same way for [`PowerDirection::Export`]
    ///   as it does for `Import` - a limit is a magnitude, not an import-only concept, so
    ///   suspending discharge is `Some(0)` too, never a separate signal. Callers must not conflate
    ///   any of this with `None`, or with an open contactor; that distinction is the whole point
    ///   of this hook existing.
    /// - `Some(n)` below the nominal current draws at `n` instead; a limit at or above nominal has
    ///   no effect, since the simulated hardware can't exceed its own nominal rate regardless of
    ///   what the CSMS allows.
    ///
    /// The clamp is always applied to the unsigned magnitude and the sign from `direction` is
    /// applied afterward - never the other way around - so a limit can never be bypassed by which
    /// direction power happens to be flowing.
    ///
    /// Power ([`MeterSample::power_w`]) is signed (decision 4): positive is import, negative is
    /// export. `direction` says which one is currently in effect for this tick - see
    /// [`PowerDirection`]'s doc comment for why that is a per-tick argument rather than something
    /// fixed on this type.
    ///
    /// **Energy accounting.** [`MeterSample::energy_wh`] mirrors OCPP's
    /// `Energy.Active.Import.Register` (see upstream's doc comment on that field) - a register,
    /// not a net total, so it must never run backwards. It therefore only ever accrues while
    /// `direction` is `Import`; while `Export` is active it is left exactly as it was, not
    /// decremented and not summed with signed power, either of which would misreport what a real
    /// import register does. Exported energy accrues separately into a value reachable through
    /// [`Self::exported_energy_wh`] - see [`MeterState::exported_energy_wh`]'s doc comment for why
    /// that isn't threaded through `MeterSample` itself yet.
    ///
    /// `soc_percent` is always `None`: there is no vehicle model in the hardware layer yet, and
    /// fabricating a state of charge would violate "never advertise what isn't simulated".
    /// `charger/state.rs`'s vehicle model (`session_duration`/`Vehicle::state_of_charge`)
    /// deliberately stayed put in H3b for exactly this reason - moving it needs a simulated
    /// battery down here that doesn't exist yet.
    pub fn tick(
        &self,
        elapsed: Duration,
        contactor_closed: bool,
        current_limit_ma: Option<u32>,
        direction: PowerDirection,
    ) -> MeterSample {
        let mut state = self.state.lock().expect("lock poisoned");

        // Magnitude only - the clamp has no notion of direction, so a limit applies identically
        // whichever way power is flowing. Sign is applied after, below.
        let magnitude_ma = if contactor_closed {
            match current_limit_ma {
                Some(limit) => nominal_current_ma().min(limit as f64),
                None => nominal_current_ma(),
            }
        } else {
            0.0
        };
        let magnitude_power_w = magnitude_ma / 1_000.0 * NOMINAL_VOLTAGE;
        let energy_delta_wh = magnitude_power_w * (elapsed.as_secs_f64() / 3_600.0);

        let (signed_current_ma, signed_power_w) = match direction {
            PowerDirection::Import => {
                state.imported_energy_wh += energy_delta_wh;
                (magnitude_ma, magnitude_power_w)
            }
            PowerDirection::Export => {
                state.exported_energy_wh += energy_delta_wh;
                (-magnitude_ma, -magnitude_power_w)
            }
        };

        MeterSample {
            energy_wh: state.imported_energy_wh.round() as i64,
            power_w: Some(signed_power_w.round() as i64),
            current_ma: Some(signed_current_ma.round() as i64),
            voltage_v: Some(NOMINAL_VOLTAGE.round() as i64),
            soc_percent: None,
        }
    }

    /// Cumulative *exported* energy so far, in Wh - the discharge counterpart to
    /// [`MeterSample::energy_wh`]'s import register. See [`MeterState::exported_energy_wh`]'s doc
    /// comment for why this lives as its own accessor rather than a `MeterSample` field: upstream's
    /// struct has no `Energy.Active.Export.Register` slot to report it through yet.
    pub fn exported_energy_wh(&self) -> i64 {
        self.state
            .lock()
            .expect("lock poisoned")
            .exported_energy_wh
            .round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nominal_hourly_energy_wh() -> i64 {
        (SIMULATED_CHARGING_POWER_KW * 1000.0).round() as i64
    }

    #[test]
    fn a_closed_contactor_accrues_energy_at_the_nominal_rate() {
        let meter = SimulatedMeter::new();
        let sample = meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Import,
        );

        assert_eq!(sample.energy_wh, nominal_hourly_energy_wh());
        assert_eq!(sample.power_w, Some(7_400));
        assert!(sample.current_ma.unwrap() > 0);
    }

    #[test]
    fn an_open_contactor_accrues_no_energy() {
        let meter = SimulatedMeter::new();
        let sample = meter.tick(
            Duration::from_secs(3600),
            false,
            None,
            PowerDirection::Import,
        );

        assert_eq!(sample.energy_wh, 0);
        assert_eq!(sample.power_w, Some(0));
        assert_eq!(sample.current_ma, Some(0));
    }

    #[test]
    fn some_zero_limit_halts_accrual_but_is_not_a_fault() {
        let meter = SimulatedMeter::new();
        let sample = meter.tick(
            Duration::from_secs(3600),
            true,
            Some(0),
            PowerDirection::Import,
        );

        assert_eq!(sample.energy_wh, 0);
        assert_eq!(sample.power_w, Some(0));
        assert_eq!(sample.current_ma, Some(0));
    }

    #[test]
    fn none_after_a_limit_restores_the_nominal_rate() {
        let meter = SimulatedMeter::new();
        let limited = meter.tick(
            Duration::from_secs(3600),
            true,
            Some(1_000),
            PowerDirection::Import,
        );
        let restored = meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Import,
        );

        // The limited hour accrued far less than nominal...
        assert!(limited.energy_wh < nominal_hourly_energy_wh());
        // ...but the very next hour, with the limit lifted, accrues a full nominal hour's worth
        // on top of it - not a continuation of the limited rate.
        assert_eq!(
            restored.energy_wh - limited.energy_wh,
            nominal_hourly_energy_wh()
        );
    }

    #[test]
    fn a_limit_at_half_the_current_yields_roughly_half_the_energy() {
        let low = SimulatedMeter::new().tick(
            Duration::from_secs(3600),
            true,
            Some(8_000),
            PowerDirection::Import,
        );
        let high = SimulatedMeter::new().tick(
            Duration::from_secs(3600),
            true,
            Some(16_000),
            PowerDirection::Import,
        );

        let ratio = low.energy_wh as f64 / high.energy_wh as f64;
        assert!((ratio - 0.5).abs() < 0.01, "ratio was {ratio}");
    }

    #[test]
    fn splitting_the_same_elapsed_time_across_many_small_ticks_matches_one_large_tick() {
        let single = SimulatedMeter::new().tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Import,
        );

        let many = SimulatedMeter::new();
        let mut last = MeterSample::default();
        for _ in 0..100 {
            last = many.tick(
                Duration::from_millis(36_000),
                true,
                None,
                PowerDirection::Import,
            );
        }

        assert!(
            (single.energy_wh - last.energy_wh).abs() <= 1,
            "single tick = {}, split across 100 ticks = {}",
            single.energy_wh,
            last.energy_wh
        );
    }

    #[test]
    fn soc_percent_is_never_fabricated() {
        let meter = SimulatedMeter::new();
        let sample = meter.tick(Duration::from_secs(60), true, None, PowerDirection::Import);

        assert_eq!(sample.soc_percent, None);
    }

    // --- H14a: discharge (V2G export) ---

    #[test]
    fn a_discharging_connector_reports_negative_power_and_current() {
        let meter = SimulatedMeter::new();
        let sample = meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Export,
        );

        assert_eq!(sample.power_w, Some(-7_400));
        assert!(sample.current_ma.unwrap() < 0);
    }

    #[test]
    fn discharging_accrues_exported_energy_while_leaving_the_import_register_at_zero() {
        let meter = SimulatedMeter::new();
        let sample = meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Export,
        );

        // The import register - what `MeterSample::energy_wh` actually is - never moves for an
        // export tick: it wasn't imported.
        assert_eq!(sample.energy_wh, 0);
        // The energy didn't vanish - it accrued into the separate export accumulator.
        assert_eq!(meter.exported_energy_wh(), nominal_hourly_energy_wh());
    }

    #[test]
    fn the_import_register_never_decreases_while_discharging() {
        let meter = SimulatedMeter::new();
        let charged = meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Import,
        );
        assert_eq!(charged.energy_wh, nominal_hourly_energy_wh());

        // An hour of discharge afterward must not erase, decrement, or otherwise touch what was
        // already imported - a real Energy.Active.Import.Register cannot run backwards.
        let discharged = meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Export,
        );
        assert_eq!(discharged.energy_wh, charged.energy_wh);
    }

    #[test]
    fn some_zero_limit_suspends_discharge_without_ending_the_session() {
        let meter = SimulatedMeter::new();
        let sample = meter.tick(
            Duration::from_secs(3600),
            true,
            Some(0),
            PowerDirection::Export,
        );

        assert_eq!(sample.power_w, Some(0));
        assert_eq!(sample.current_ma, Some(0));
        assert_eq!(meter.exported_energy_wh(), 0);
    }

    #[test]
    fn none_after_a_discharge_limit_restores_the_full_discharge_rate() {
        let meter = SimulatedMeter::new();
        meter.tick(
            Duration::from_secs(3600),
            true,
            Some(1_000),
            PowerDirection::Export,
        );
        let limited_export = meter.exported_energy_wh();
        assert!(limited_export < nominal_hourly_energy_wh());

        meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Export,
        );
        // The next hour, unlimited, accrues a full nominal hour's worth on top - not a
        // continuation of the limited rate.
        assert_eq!(
            meter.exported_energy_wh() - limited_export,
            nominal_hourly_energy_wh()
        );
    }

    #[test]
    fn a_discharge_limit_at_half_the_current_yields_roughly_half_the_exported_energy() {
        let low = SimulatedMeter::new();
        low.tick(
            Duration::from_secs(3600),
            true,
            Some(8_000),
            PowerDirection::Export,
        );
        let high = SimulatedMeter::new();
        high.tick(
            Duration::from_secs(3600),
            true,
            Some(16_000),
            PowerDirection::Export,
        );

        let ratio = low.exported_energy_wh() as f64 / high.exported_energy_wh() as f64;
        assert!((ratio - 0.5).abs() < 0.01, "ratio was {ratio}");
    }

    #[test]
    fn switching_direction_mid_session_freezes_the_side_that_is_not_active() {
        let meter = SimulatedMeter::new();

        // Charge for an hour...
        let after_charge = meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Import,
        );
        assert_eq!(after_charge.energy_wh, nominal_hourly_energy_wh());
        assert_eq!(meter.exported_energy_wh(), 0);

        // ...then discharge for an hour: import register holds still, export starts from zero.
        let after_discharge = meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Export,
        );
        assert_eq!(after_discharge.energy_wh, nominal_hourly_energy_wh());
        assert_eq!(meter.exported_energy_wh(), nominal_hourly_energy_wh());

        // ...then back to charging: import resumes from where it was left, not from zero.
        let resumed = meter.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Import,
        );
        assert_eq!(resumed.energy_wh, nominal_hourly_energy_wh() * 2);
        assert_eq!(meter.exported_energy_wh(), nominal_hourly_energy_wh());
    }

    #[test]
    fn splitting_the_same_elapsed_time_across_many_small_ticks_matches_one_large_tick_while_discharging()
     {
        let single = SimulatedMeter::new();
        single.tick(
            Duration::from_secs(3600),
            true,
            None,
            PowerDirection::Export,
        );

        let many = SimulatedMeter::new();
        for _ in 0..100 {
            many.tick(
                Duration::from_millis(36_000),
                true,
                None,
                PowerDirection::Export,
            );
        }

        assert!(
            (single.exported_energy_wh() - many.exported_energy_wh()).abs() <= 1,
            "single tick = {}, split across 100 ticks = {}",
            single.exported_energy_wh(),
            many.exported_energy_wh()
        );
    }
}
