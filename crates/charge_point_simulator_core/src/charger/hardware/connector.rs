use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ocpp_charge_point::hardware::Connector;
use ocpp_charge_point::state::MeterSample;

use super::metering::SimulatedMeter;

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
    /// This connector's simulated meter, advanced by [`Self::tick`] using
    /// [`Self::is_contactor_closed`] and [`Self::current_limit_ma`] as its inputs.
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

    /// Advances this connector's simulated meter by `elapsed`, reading the contactor and current
    /// limit state tracked right here as the meter's inputs - see [`SimulatedMeter::tick`] for
    /// the physics. Nothing calls this yet; it exists for
    /// [`super::charge_point::FakeChargePoint::tick`] to drive per connector - which addresses
    /// the resulting sample by this connector's *position* among its EVSE's connectors, not by
    /// `evse_id`/`connector_id` here (those exist for `tracing` only, and carry the config's
    /// possibly non-contiguous EVSE/connector numbering rather than the array index the
    /// OCPP-facing state machine addresses by).
    pub fn tick(&self, elapsed: Duration) -> MeterSample {
        self.meter
            .tick(elapsed, self.is_contactor_closed(), self.current_limit_ma())
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
}
