use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ocpp_charge_point::hardware::Connector;
use ocpp_charge_point::state::MeterSample;

use super::metering::{PowerDirection, SimulatedMeter};

/// A simulated connector actuator: no real hardware behind it, just lock/contactor/meter
/// state tracked in memory. Every actuator action is reported via `tracing` so it flows into
/// whatever is bridging tracing output (e.g. the TUI's log panel) the same way real
/// hardware driver logs would.
#[derive(Debug)]
pub struct FakeConnector {
    evse_id: usize,
    connector_id: usize,
    locked: AtomicBool,
    contactor_closed: AtomicBool,
    /// The most recent current limit applied via [`Connector::set_current_limit`], in mA, or
    /// `None` when no CSMS-imposed limit currently applies. Read by [`Self::tick`] on every call
    /// to clamp [`Self::meter`]'s nominal current draw - see [`SimulatedMeter::tick`] for the
    /// `Some(0)` vs `None` semantics.
    current_limit_ma: Mutex<Option<u32>>,
    /// Whether this connector is currently discharging (exporting power back to the grid, V2G)
    /// rather than importing it. Mutable session state, not a fixed trait of the hardware - see
    /// [`PowerDirection`]'s doc comment for why: a connector capable of export doesn't always
    /// export, and which way it's going can change mid-transaction. Lives here next to
    /// [`Self::current_limit_ma`], the same shape, and is read by [`Self::tick`] on every call.
    ///
    /// Nothing outside this connector's own tests flips this yet - wiring an actual OCPP trigger
    /// (a DER control setpoint, most likely) to [`Self::set_discharging`] is H14b's job, tracked
    /// separately in `docs/hardware-roadmap.md`. That registration work is deliberately not this
    /// task's to do.
    discharging: AtomicBool,
    /// This connector's simulated meter, advanced by [`Self::tick`] using
    /// [`Self::is_contactor_closed`], [`Self::current_limit_ma`] and [`Self::is_discharging`] as
    /// its inputs.
    meter: SimulatedMeter,
}

impl FakeConnector {
    pub fn new(evse_id: usize, connector_id: usize) -> Self {
        Self {
            evse_id,
            connector_id,
            locked: AtomicBool::new(false),
            contactor_closed: AtomicBool::new(false),
            current_limit_ma: Mutex::new(None),
            discharging: AtomicBool::new(false),
            meter: SimulatedMeter::new(),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    pub fn is_contactor_closed(&self) -> bool {
        self.contactor_closed.load(Ordering::Relaxed)
    }

    /// The current limit last applied to this connector, in mA, or `None` if unlimited.
    pub fn current_limit_ma(&self) -> Option<u32> {
        *self.current_limit_ma.lock().expect("lock poisoned")
    }

    /// Whether this connector is currently discharging (exporting power) rather than importing
    /// it. `false` - importing - until [`Self::set_discharging`] says otherwise.
    pub fn is_discharging(&self) -> bool {
        self.discharging.load(Ordering::Relaxed)
    }

    /// Switches this connector between importing (`false`) and discharging/exporting (`true`)
    /// power, effective on the next [`Self::tick`]. Session state, not a one-time hardware
    /// configuration - see the field doc comment on [`Self::discharging`].
    pub fn set_discharging(&self, discharging: bool) {
        self.discharging.store(discharging, Ordering::Relaxed);
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            discharging,
            "power direction set"
        );
    }

    /// Cumulative energy this connector has exported (discharged back to the grid) so far, in
    /// Wh - see [`SimulatedMeter::exported_energy_wh`] for why this is a separate reading from
    /// any [`MeterSample`] this connector emits, rather than folded into `energy_wh`.
    pub fn exported_energy_wh(&self) -> i64 {
        self.meter.exported_energy_wh()
    }

    /// Advances this connector's simulated meter by `elapsed`, reading the contactor, current
    /// limit and discharge-direction state tracked right here as the meter's inputs - see
    /// [`SimulatedMeter::tick`] for the physics. Driven per connector by
    /// [`super::charge_point::FakeChargePoint::tick`], which addresses the resulting sample by
    /// this connector's *position* among its EVSE's connectors, not by `evse_id`/`connector_id`
    /// here (those exist for `tracing` only, and carry the config's possibly non-contiguous
    /// EVSE/connector numbering rather than the array index the OCPP-facing state machine
    /// addresses by).
    pub fn tick(&self, elapsed: Duration) -> MeterSample {
        let direction = if self.is_discharging() {
            PowerDirection::Export
        } else {
            PowerDirection::Import
        };
        self.meter.tick(
            elapsed,
            self.is_contactor_closed(),
            self.current_limit_ma(),
            direction,
        )
    }
}

#[async_trait::async_trait]
impl Connector for FakeConnector {
    type Error = core::convert::Infallible;

    async fn lock(&self) -> Result<(), Self::Error> {
        self.locked.store(true, Ordering::Relaxed);
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            "connector locked"
        );
        Ok(())
    }

    async fn unlock(&self) -> Result<(), Self::Error> {
        self.locked.store(false, Ordering::Relaxed);
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            "connector unlocked"
        );
        Ok(())
    }

    async fn close_contactor(&self) -> Result<(), Self::Error> {
        self.contactor_closed.store(true, Ordering::Relaxed);
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            "contactor closed"
        );
        Ok(())
    }

    async fn open_contactor(&self) -> Result<(), Self::Error> {
        self.contactor_closed.store(false, Ordering::Relaxed);
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            "contactor opened"
        );
        Ok(())
    }

    async fn set_current_limit(&self, limit_ma: Option<u32>) -> Result<(), Self::Error> {
        *self.current_limit_ma.lock().expect("lock poisoned") = limit_ma;
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            limit_ma = ?limit_ma,
            "current limit set"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lock_and_unlock_flip_the_locked_flag() {
        let connector = FakeConnector::new(1, 1);
        assert!(!connector.is_locked());

        connector.lock().await.unwrap();
        assert!(connector.is_locked());

        connector.unlock().await.unwrap();
        assert!(!connector.is_locked());
    }

    #[tokio::test]
    async fn close_and_open_contactor_flip_the_contactor_flag() {
        let connector = FakeConnector::new(1, 1);
        assert!(!connector.is_contactor_closed());

        connector.close_contactor().await.unwrap();
        assert!(connector.is_contactor_closed());

        connector.open_contactor().await.unwrap();
        assert!(!connector.is_contactor_closed());
    }

    #[tokio::test]
    async fn set_current_limit_records_the_limit_and_clears_it_again() {
        let connector = FakeConnector::new(1, 1);
        assert_eq!(connector.current_limit_ma(), None);

        connector.set_current_limit(Some(16_000)).await.unwrap();
        assert_eq!(connector.current_limit_ma(), Some(16_000));

        // `Some(0)` is "suspend charging", distinct from `None` - both must round-trip.
        connector.set_current_limit(Some(0)).await.unwrap();
        assert_eq!(connector.current_limit_ma(), Some(0));

        connector.set_current_limit(None).await.unwrap();
        assert_eq!(connector.current_limit_ma(), None);
    }

    #[tokio::test]
    async fn tick_reads_this_connectors_own_contactor_and_limit_state() {
        let connector = FakeConnector::new(1, 1);

        // Contactor open: no accrual, regardless of nothing else having been touched.
        let sample = connector.tick(Duration::from_secs(3600));
        assert_eq!(sample.energy_wh, 0);

        connector.close_contactor().await.unwrap();
        let sample = connector.tick(Duration::from_secs(3600));
        assert!(sample.energy_wh > 0);
    }

    #[tokio::test]
    async fn some_zero_limit_leaves_the_connector_itself_untouched() {
        let connector = FakeConnector::new(1, 1);
        connector.close_contactor().await.unwrap();
        connector.set_current_limit(Some(0)).await.unwrap();

        let sample = connector.tick(Duration::from_secs(3600));

        assert_eq!(sample.energy_wh, 0);
        // Suspended, not faulted: the contactor is still reported closed and the limit still
        // reads back as `Some(0)`, not cleared or coerced into anything else.
        assert!(connector.is_contactor_closed());
        assert_eq!(connector.current_limit_ma(), Some(0));
    }

    // --- H14a: discharge (V2G export) ---

    #[tokio::test]
    async fn a_new_connector_imports_by_default() {
        let connector = FakeConnector::new(1, 1);
        assert!(!connector.is_discharging());
    }

    #[tokio::test]
    async fn set_discharging_flips_the_direction_and_back() {
        let connector = FakeConnector::new(1, 1);

        connector.set_discharging(true);
        assert!(connector.is_discharging());

        connector.set_discharging(false);
        assert!(!connector.is_discharging());
    }

    #[tokio::test]
    async fn a_discharging_connector_reports_negative_power_and_current_through_tick() {
        let connector = FakeConnector::new(1, 1);
        connector.close_contactor().await.unwrap();
        connector.set_discharging(true);

        let sample = connector.tick(Duration::from_secs(3600));

        assert!(sample.power_w.unwrap() < 0);
        assert!(sample.current_ma.unwrap() < 0);
        // Nothing was imported this tick...
        assert_eq!(sample.energy_wh, 0);
        // ...but the energy didn't vanish - it shows up in the connector's own export reading.
        assert!(connector.exported_energy_wh() > 0);
    }

    #[tokio::test]
    async fn some_zero_limit_suspends_discharge_without_ending_the_session_or_faulting() {
        let connector = FakeConnector::new(1, 1);
        connector.close_contactor().await.unwrap();
        connector.set_discharging(true);
        connector.set_current_limit(Some(0)).await.unwrap();

        let sample = connector.tick(Duration::from_secs(3600));

        assert_eq!(sample.power_w, Some(0));
        assert_eq!(sample.current_ma, Some(0));
        // Suspended, not faulted or stopped: contactor still closed, still discharging, limit
        // still reads back as `Some(0)` rather than being cleared or coerced.
        assert!(connector.is_contactor_closed());
        assert!(connector.is_discharging());
        assert_eq!(connector.current_limit_ma(), Some(0));
    }

    #[tokio::test]
    async fn switching_direction_mid_session_behaves_sanely_through_the_connector() {
        let connector = FakeConnector::new(1, 1);
        connector.close_contactor().await.unwrap();

        // Import for an hour...
        let imported = connector.tick(Duration::from_secs(3600));
        assert!(imported.energy_wh > 0);

        // ...then switch to export for an hour: the import register must not move, and power
        // must flip sign.
        connector.set_discharging(true);
        let exported = connector.tick(Duration::from_secs(3600));
        assert_eq!(exported.energy_wh, imported.energy_wh);
        assert!(exported.power_w.unwrap() < 0);

        // ...then back to import: the register resumes climbing from where it left off.
        connector.set_discharging(false);
        let resumed = connector.tick(Duration::from_secs(3600));
        assert!(resumed.energy_wh > imported.energy_wh);
        assert!(resumed.power_w.unwrap() > 0);
    }
}
