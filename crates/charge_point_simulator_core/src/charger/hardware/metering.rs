use std::sync::Mutex;
use std::time::Duration;

use ocpp_charge_point::state::MeterSample;

/// Simulated charging power a connector draws while its contactor is closed and no CSMS current
/// limit clamps it further, in kW - a plausible single-phase AC rate, not derived from any real
/// hardware spec.
///
/// `charger/state.rs`'s `EvseState::SIMULATED_CHARGING_POWER_KW` is a temporary duplicate of this
/// value. It exists only until the physics-convergence task in `docs/hardware-roadmap.md` (the
/// second half of H3, deliberately out of scope here) deletes `EvseState::tick`'s copy of the
/// constants and the accumulation loop they drive, once a local (unconnected) simulation has this
/// hardware layer to route its meter samples through. This module is the canonical home for the
/// constant from that point on.
pub const SIMULATED_CHARGING_POWER_KW: f64 = 7.4;

/// Nominal single-phase voltage used to convert between the simulated power and current
/// readings. Duplicated in `charger/state.rs` for the same reason as
/// [`SIMULATED_CHARGING_POWER_KW`] - see that constant's doc comment.
pub const NOMINAL_VOLTAGE: f64 = 230.0;

/// The nominal current draw at [`SIMULATED_CHARGING_POWER_KW`]/[`NOMINAL_VOLTAGE`], in
/// milliamps - the ceiling [`SimulatedMeter::tick`] clamps against when a CSMS current limit
/// applies.
fn nominal_current_ma() -> f64 {
    SIMULATED_CHARGING_POWER_KW * 1_000.0 / NOMINAL_VOLTAGE * 1_000.0
}

/// One connector's simulated electrical state. Accumulates in `f64` (decision 3 in the hardware
/// roadmap's "Decisions taken") and is converted to [`MeterSample`]'s integer fields only at the
/// point of emission in [`SimulatedMeter::tick`] - never inside the accumulator itself, so
/// splitting the same total `elapsed` across many small ticks or one large one gives the same
/// result within float tolerance (see the working agreement on accumulators coarser than their
/// increment).
#[derive(Debug, Default)]
struct MeterState {
    /// Cumulative imported energy, in Wh.
    energy_wh: f64,
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
    /// notion of "charging", which this layer cannot see. `current_limit_ma` clamps the nominal
    /// current draw:
    ///
    /// - `None` means no CSMS-imposed limit - the nominal rate applies.
    /// - `Some(0)` means suspended: the clamp floors current (and so power and energy accrual) at
    ///   zero, but this is a rate of zero, not a fault - the connector is not faulted and any
    ///   transaction is not ended. Callers must not conflate this with `None`, or with an open
    ///   contactor; that distinction is the whole point of this hook existing.
    /// - `Some(n)` below the nominal current draws at `n` instead; a limit at or above nominal has
    ///   no effect, since the simulated hardware can't exceed its own nominal rate regardless of
    ///   what the CSMS allows.
    ///
    /// Power ([`MeterSample::power_w`]) is signed (decision 4): positive is import. Only import is
    /// simulated today - V2G export is a later task (H14) - but the sign is already meaningful so
    /// that task doesn't need a rework to express it.
    ///
    /// `soc_percent` is always `None`: there is no vehicle model in the hardware layer yet, and
    /// fabricating a state of charge would violate "never advertise what isn't simulated". Left
    /// for the physics-convergence task, once `charger/state.rs`'s vehicle model has somewhere to
    /// plug into.
    pub fn tick(
        &self,
        elapsed: Duration,
        contactor_closed: bool,
        current_limit_ma: Option<u32>,
    ) -> MeterSample {
        let mut state = self.state.lock().expect("lock poisoned");

        let current_ma = if contactor_closed {
            match current_limit_ma {
                Some(limit) => nominal_current_ma().min(limit as f64),
                None => nominal_current_ma(),
            }
        } else {
            0.0
        };

        let power_w = current_ma / 1_000.0 * NOMINAL_VOLTAGE;
        state.energy_wh += power_w * (elapsed.as_secs_f64() / 3_600.0);

        MeterSample {
            energy_wh: state.energy_wh.round() as i64,
            power_w: Some(power_w.round() as i64),
            current_ma: Some(current_ma.round() as i64),
            voltage_v: Some(NOMINAL_VOLTAGE.round() as i64),
            soc_percent: None,
        }
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
        let sample = meter.tick(Duration::from_secs(3600), true, None);

        assert_eq!(sample.energy_wh, nominal_hourly_energy_wh());
        assert_eq!(sample.power_w, Some(7_400));
        assert!(sample.current_ma.unwrap() > 0);
    }

    #[test]
    fn an_open_contactor_accrues_no_energy() {
        let meter = SimulatedMeter::new();
        let sample = meter.tick(Duration::from_secs(3600), false, None);

        assert_eq!(sample.energy_wh, 0);
        assert_eq!(sample.power_w, Some(0));
        assert_eq!(sample.current_ma, Some(0));
    }

    #[test]
    fn some_zero_limit_halts_accrual_but_is_not_a_fault() {
        let meter = SimulatedMeter::new();
        let sample = meter.tick(Duration::from_secs(3600), true, Some(0));

        assert_eq!(sample.energy_wh, 0);
        assert_eq!(sample.power_w, Some(0));
        assert_eq!(sample.current_ma, Some(0));
    }

    #[test]
    fn none_after_a_limit_restores_the_nominal_rate() {
        let meter = SimulatedMeter::new();
        let limited = meter.tick(Duration::from_secs(3600), true, Some(1_000));
        let restored = meter.tick(Duration::from_secs(3600), true, None);

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
        let low = SimulatedMeter::new().tick(Duration::from_secs(3600), true, Some(8_000));
        let high = SimulatedMeter::new().tick(Duration::from_secs(3600), true, Some(16_000));

        let ratio = low.energy_wh as f64 / high.energy_wh as f64;
        assert!((ratio - 0.5).abs() < 0.01, "ratio was {ratio}");
    }

    #[test]
    fn splitting_the_same_elapsed_time_across_many_small_ticks_matches_one_large_tick() {
        let single = SimulatedMeter::new().tick(Duration::from_secs(3600), true, None);

        let many = SimulatedMeter::new();
        let mut last = MeterSample::default();
        for _ in 0..100 {
            last = many.tick(Duration::from_millis(36_000), true, None);
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
        let sample = meter.tick(Duration::from_secs(60), true, None);

        assert_eq!(sample.soc_percent, None);
    }
}
