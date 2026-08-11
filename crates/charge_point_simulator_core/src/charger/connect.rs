use core::convert::Infallible;
use std::sync::Arc;

use ocpp_charge_point::ChargePointBuilder;
use ocpp_charge_point::ConnectAndSetupError;
use ocpp_charge_point::authorization::{Authorizer, ClearCacheHandler};
use ocpp_charge_point::availability::{ChangeAvailabilityHandler, StatusNotifier};
use ocpp_charge_point::certificates::CertificateHandler;
use ocpp_charge_point::clock::{Clock, MonotonicClock, SystemClock, SystemMonotonicClock};
use ocpp_charge_point::connection::ReconnectHandler;
use ocpp_charge_point::cost::CostUpdatedHandler;
use ocpp_charge_point::device_model::{GetVariablesHandler, SetVariablesHandler};
use ocpp_charge_point::diagnostics::{GetLogHandler, LogStatusNotifier};
use ocpp_charge_point::display_message::{
    ClearDisplayMessageHandler, GetDisplayMessagesHandler, SetDisplayMessageHandler,
};
use ocpp_charge_point::executor::{Executor, TokioExecutor};
use ocpp_charge_point::firmware::{
    FirmwareStatusNotifier, SignedUpdateFirmwareHandler, UpdateFirmwareHandler,
};
use ocpp_charge_point::hardware::{
    ChargePoint, Connector, Display, Evse, NoFirmwareVerifier, Storage,
};
use ocpp_charge_point::local_authorization_list::{
    GetLocalListVersionHandler, SendLocalListHandler,
};
use ocpp_charge_point::meter_values::MeterValuesNotifier;
use ocpp_charge_point::network_profile::SetNetworkProfileHandler;
use ocpp_charge_point::network_switch::ConnectionTarget;
use ocpp_charge_point::payload_limit::PayloadLimits;
use ocpp_charge_point::periodic_event_stream::{
    AdjustPeriodicEventStreamHandler, ClosePeriodicEventStreamHandler,
    GetPeriodicEventStreamHandler, OpenPeriodicEventStreamHandler, PeriodicEventStreamNotifier,
};
use ocpp_charge_point::persistence::{QueueStore, SecurityLogStore};
use ocpp_charge_point::provisioning::{Backoff, BootNotifier, HeartbeatSender, TokioBackoff};
use ocpp_charge_point::remote_control::{
    RequestStartTransactionHandler, RequestStopTransactionHandler, TriggerMessageHandler,
    UnlockConnectorHandler,
};
use ocpp_charge_point::reporting::{GetBaseReportHandler, GetReportHandler};
use ocpp_charge_point::reservation::{
    CancelReservationHandler, ReservationStatusNotifier, ReserveNowHandler,
};
use ocpp_charge_point::reset::ResetHandler;
use ocpp_charge_point::security::{SecurityEventLog, SecurityEventNotifier};
use ocpp_charge_point::smart_charging::{
    ChargingLimitProjection, ClearChargingProfileHandler, GetChargingProfilesHandler,
    GetCompositeScheduleHandler, SetChargingProfileHandler,
};
use ocpp_charge_point::tariff::{
    ChangeTransactionTariffHandler, ClearTariffsHandler, GetTariffsHandler, SetDefaultTariffHandler,
};
use ocpp_charge_point::transactions::TransactionNotifier;
use ocpp_charge_point::variable_monitoring::{
    ClearVariableMonitoringHandler, GetMonitoringReportHandler, SetMonitoringBaseHandler,
    SetMonitoringLevelHandler, SetVariableMonitoringHandler, VariableMonitorEventNotifier,
};
use ocpp_client::{ConnectOptions, NegotiatedClient, OcppVersion};

use super::config::ChargerConfig;
use super::connection::{ConnectionProfile, SecurityProfile};
use super::hardware::{
    FakeChargePoint, FakeDisplay, FakeFileTransfer, FakeFirmwareInstaller, FakeFirmwareVerifier,
    FileCertificateStore, FileStorage,
};
use super::hardware_bundle::ChargerHardware;
use super::running_charger::RunningCharger;

/// Builds the WebSocket URL to dial for `ocpp_identity`, given the CSMS's configured base
/// address: normalizes an `http(s)://` scheme to `ws(s)://` (so [`ConnectionProfile::csms_url`]
/// can be written either way) and appends the identity as the final path segment.
pub fn websocket_url(base_url: &str, ocpp_identity: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    let base_url = if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base_url.to_string()
    };
    format!("{base_url}/{ocpp_identity}")
}

/// Registers every functional block `ocpp_charge_point::setup`'s "everything on" wrapper does,
/// in the same order and under the same [`Capabilities`](ocpp_charge_point::hardware::Capabilities)
/// gating that function's own source uses - 13 registrations made unconditionally, then 11 more
/// gated in six `if capabilities.*` blocks (reservation +
/// reservation_status_updates; local_authorization_list; cost + tariffs; smart_charging +
/// charging_profile_reports; variable_monitoring + monitoring_reports +
/// variable_monitor_events; periodic_event_streams) - plus, beyond what `setup()` itself knows how
/// to do at all (see below), storage-backed persistence and display messages, each gated on its
/// own `Capabilities` flag and on `hardware` actually supplying the matching object.
///
/// Unlike `setup()`, this stops short of `offline_queue_retries`/`build()`: the whole reason
/// `connect_charger` drives the builder by hand instead of calling `setup()` is that `setup()`
/// seals the builder into a [`ChargePointRuntime`] before returning, with no hook left to add a
/// registration afterwards. Returning the still-open builder is what lets `connect_charger` add
/// OCPP 2.1's extra blocks `setup()` cannot know about (see its caller).
///
/// # Return value (`docs/hardware-roadmap.md` decision 8)
///
/// Returns `(builder, security_log)` rather than just the builder. `security_log` is the same
/// [`Arc<SecurityEventLog>`](SecurityEventLog) handle `security_log_persisted` restored persisted
/// history into - `Some` exactly when `storage` was declared and present (the same condition that
/// registers `security_log_persisted` at all), `None` otherwise. It exists so
/// [`register_optional_hardware`]'s `log_uploads` can share it instead of being handed a fresh,
/// empty log: before this, `log_uploads` had no way to reach the restored handle at all (it lives
/// entirely inside this function's own stack frame), so an uploaded security log silently omitted
/// everything from before the last restart. Every caller of this function should thread its
/// `security_log` straight into [`register_optional_hardware`], whether or not it ends up calling
/// that function's `diagnostics`-gated branch.
///
/// # Persistence (H5b) and display (H6b)
///
/// `setup()` has no `Storage`/`Display` parameter at all - it cannot persist anything or drive a
/// screen, on any capability. Reaching either is the entire reason this function exists instead of
/// calling `setup()` directly (H2). `storage`/`display` come from [`ChargerHardware`]; each
/// persistence/display registration only fires when **both** the matching `Capabilities` flag is
/// set and the object is actually present - `storage`/`display` are pre-filtered by capability
/// once, at the top, specifically so every call site below can just check `is_some()` without
/// re-deriving that combination. A capability declared `true` with nothing behind it logs a
/// warning and registers nothing, rather than panicking on a method call it has no value to make.
///
/// Three of the nine storage-backed registrations upstream exposes -
/// `status_notifications`/`transaction_events`/`security_events` each have a `_persisted`
/// sibling - are **not** simply added alongside the plain calls above: each plain/persisted pair
/// consumes the same single-use broadcast subscription (`take_status_changes`/
/// `take_transaction_events`/`take_security_events` on the builder), so calling both would not
/// double-register anything loud - the second call finds the subscription already taken and
/// silently no-ops, which is a *worse* failure than a compile error, because persistence would
/// appear registered and simply never run. `setup()` itself only ever calls the plain form (it has
/// no storage to hand a `_persisted` call anyway), so this function keeps that as the default and
/// switches to the `_persisted` form - never both - exactly when persistence is available, per
/// block. The other six storage-backed methods
/// (`boot_reason_persistence`/`transaction_persistence`/`authorization_cache_persistence`/
/// `network_profile_persistence`/`device_model_persistence`/`security_log_persisted`) and the
/// three that are additionally gated behind their own functional block's capability
/// (`reservation_persistence`/`local_authorization_list_persistence`/
/// `charging_profile_persistence`) have no such conflict - each is documented as safe to call
/// alongside its sibling registration, independent of every other `*_persistence` method - so they
/// are simply added, ordered per each method's own "call this before ..." doc requirement (e.g.
/// `boot_reason_persistence` before `provisioning`, `authorization_cache_persistence` before
/// `authorization`).
///
/// Generic over the CSMS client `N`, exactly like `setup()` is, so this same function - not a
/// reimplementation of it - is what the equivalence test below runs against a fake CSMS. Also
/// generic over the storage/display types (`S`/`D`), rather than fixed to `FileStorage`/
/// `FakeDisplay`, for the same reason: the equivalence tests below substitute a call-recording
/// fake storage to observe *which* persistence blocks actually registered, the same way
/// `RecordingCsms` does for CSMS-facing ones - a signal plain vs. `_persisted` registration has no
/// other way to expose, since neither ever calls a `register_*` method distinguishable from the
/// other (see the doc comment above).
///
/// # `has_csms` (`docs/hardware-roadmap.md` H3c)
///
/// `false` for [`super::running_charger::start_local_charger`], `true` (unconditionally, matching
/// every call site before H3c) for [`connect_ocpp_2_1`]. Gates the blocks whose entire purpose is
/// talking to a CSMS that, in local mode, does not exist: `provisioning` (which would otherwise
/// block forever inside `register_until_accepted` - see [`NullCsms`]'s doc comment for why it must
/// never fabricate an accepted registration), the plain/`_persisted` `status_notifications`/
/// `transaction_events`/`security_events` pair, `meter_values`, and - within their own capability
/// gates - `tariff_and_cost`/`variable_monitoring`/`periodic_event_stream`. Every one of those is
/// "at best a no-op offline" (`docs/hardware-roadmap.md`'s own phrase): each either forwards a
/// locally-known fact outward with nothing that reads it back, or - `provisioning` - would hang the
/// caller.
///
/// Left ungated (registered in both modes, exactly the same code path either way): `authorization`
/// (the fix - see [`NullCsms::authorize`]), `clear_cache`/`network_profiles`/`remote_control`/
/// `trigger_message`/`availability_control`/`reset`/`device_model` (each a single non-blocking
/// handler registration with no further consequence, cheap enough to keep the functional-block
/// shape complete even though nothing ever dials in to trigger them locally), and the capability
/// -gated `reservation`(+`reservation_status_updates`)/`local_authorization_list`/`smart_charging`
/// (+`charging_profile_reports`) blocks - these three are `docs/hardware-roadmap.md`'s point:
/// `smart_charging`'s projection loops and `reservation_status_updates`'s expiry sweep react to
/// *locally* injected events (`ChargePointEvent::ChargingProfileSet`, an aged `Reservation`), not a
/// CSMS round trip, so local mode gets the same charging-limit and reservation-expiry behavior a
/// connected charger would.
// The eighth parameter (`has_csms`) is what H3c added; splitting the caller-supplied primitives
// (`backoff`/`monotonic`/`clock`/`storage`/`display`) into a struct to appease this lint would
// only add a type nothing else needs, for a private, single-purpose function with exactly two
// call sites (`connect_ocpp_2_1`, `start_local_charger`) that both already spell out every
// argument by name.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn register_setup_blocks<T, E, C, N, X, B, M, K, S, D>(
    mut builder: ChargePointBuilder<T, X>,
    csms: &N,
    backoff: B,
    monotonic: M,
    clock: K,
    storage: Option<&S>,
    display: Option<D>,
    has_csms: bool,
) -> (ChargePointBuilder<T, X>, Option<Arc<SecurityEventLog>>)
where
    T: ChargePoint<E, C>,
    E: Evse<C>,
    C: Connector,
    N: BootNotifier
        + ClearCacheHandler
        + SetNetworkProfileHandler
        + HeartbeatSender
        + StatusNotifier
        + TransactionNotifier
        + Authorizer
        + UnlockConnectorHandler
        + ChangeAvailabilityHandler
        + RequestStartTransactionHandler
        + RequestStopTransactionHandler
        + TriggerMessageHandler
        + ReserveNowHandler
        + CancelReservationHandler
        + ReservationStatusNotifier
        + ResetHandler
        + SendLocalListHandler
        + GetLocalListVersionHandler
        + GetVariablesHandler
        + SetVariablesHandler
        + GetBaseReportHandler
        + GetReportHandler
        + SecurityEventNotifier
        + CostUpdatedHandler
        + SetDefaultTariffHandler
        + ChangeTransactionTariffHandler
        + ClearTariffsHandler
        + GetTariffsHandler
        + MeterValuesNotifier
        + SetChargingProfileHandler
        + ClearChargingProfileHandler
        + GetCompositeScheduleHandler
        + GetChargingProfilesHandler
        + SetVariableMonitoringHandler
        + ClearVariableMonitoringHandler
        + VariableMonitorEventNotifier
        + SetMonitoringBaseHandler
        + SetMonitoringLevelHandler
        + GetMonitoringReportHandler
        + OpenPeriodicEventStreamHandler
        + ClosePeriodicEventStreamHandler
        + AdjustPeriodicEventStreamHandler
        + GetPeriodicEventStreamHandler
        + PeriodicEventStreamNotifier
        + ReconnectHandler
        + SetDisplayMessageHandler
        + GetDisplayMessagesHandler
        + ClearDisplayMessageHandler
        + Clone
        + Send
        + Sync
        + 'static,
    X: Executor,
    B: Backoff + Clone + Send + Sync + 'static,
    M: MonotonicClock + Clone + Send + Sync + 'static,
    K: Clock + Clone + Send + Sync + 'static,
    S: Storage + Clone + Send + Sync + 'static,
    D: Display + Send + Sync + 'static,
{
    // C3.1 upstream: each of these blocks only registers when the hardware actually declared the
    // matching capability - an absent capability means the CSMS gets `NotImplemented` rather than
    // a handler backed by hardware that can't do the thing. Read once, up front: `capabilities()`
    // only reads a field the builder cached at `start()`, so nothing below it needs to have run
    // yet for this to be accurate.
    let capabilities = builder.capabilities();
    if capabilities.has_persistent_storage && storage.is_none() {
        tracing::warn!(
            "the charger declares has_persistent_storage but ChargerHardware has no storage - \
             persistence will not be registered"
        );
    }
    if capabilities.has_display && display.is_none() {
        tracing::warn!(
            "the charger declares has_display but ChargerHardware has no display - display \
             messages will not be registered"
        );
    }
    // Pre-filtered by capability, once - see the doc comment above. Every later `if let
    // Some(storage) = storage` below is therefore already "declared and backed", with no
    // capability check duplicated at the call site.
    let storage = storage.filter(|_| capabilities.has_persistent_storage);
    let display = display.filter(|_| capabilities.has_display);

    if let Some(storage) = storage {
        builder = builder.boot_reason_persistence(storage.clone()).await;
    }
    // `has_csms`: `provisioning` calls `register_until_accepted`, which retries - with a
    // backoff, but with no upper bound - until the CSMS accepts registration. Against a
    // `NullCsms` that (correctly, per its own doc comment) never fabricates acceptance, that call
    // would never return, hanging this function - and so `start_local_charger` - forever. There
    // is no gate that makes `provisioning` safe to call locally; it is simply not called.
    if has_csms {
        builder = builder.provisioning(csms, backoff.clone(), monotonic).await;
    }

    if let Some(storage) = storage {
        builder = builder
            .transaction_persistence(storage.clone(), clock.clone())
            .await;
    }
    // `has_csms`: forwards connector status / transaction lifecycle events outward; local state
    // (`ChargerState`) is populated straight from `ChargePointState`/hardware via
    // `RunningCharger::apply_state` regardless of whether this registers, so against no CSMS this
    // is exactly the "at best a no-op offline" case - a background loop that will only ever queue
    // what it can never flush.
    if has_csms {
        builder = if let Some(storage) = storage {
            builder
                .status_notifications_persisted(csms, QueueStore::new(storage.clone(), "status"))
                .await
        } else {
            builder.status_notifications(csms).await
        };
        builder = if let Some(storage) = storage {
            builder
                .transaction_events_persisted(csms, QueueStore::new(storage.clone(), "transaction"))
                .await
        } else {
            builder.transaction_events(csms).await
        };
    }

    if let Some(storage) = storage {
        builder = builder
            .authorization_cache_persistence(storage.clone())
            .await;
    }
    builder = builder.authorization(csms, clock.clone()).await;

    builder = builder.clear_cache(csms).await;

    if let Some(storage) = storage {
        builder = builder.network_profile_persistence(storage.clone()).await;
    }
    // Unlike `has_csms`'s other gated blocks, `network_profiles`/`clear_cache`/`remote_control`/
    // `trigger_message`/`availability_control`/`reset`/`device_model` below are left ungated -
    // see the doc comment above for why: each is one non-blocking `register_*_handler` call with
    // no further consequence, so registering them locally costs nothing even though nothing ever
    // dials in to trigger them.
    builder = builder.network_profiles(csms).await;

    // `has_csms`: same "at best a no-op offline" reasoning as `status_notifications`/
    // `transaction_events` above.
    if has_csms {
        builder = if let Some(storage) = storage {
            builder
                .security_events_persisted(csms, QueueStore::new(storage.clone(), "security"))
                .await
        } else {
            builder.security_events(csms).await
        };
    }
    // Captured before the move into `security_log_persisted` (decision 8) so this function can
    // hand the same restored handle back to its caller - see the doc comment above.
    let mut security_log = None;
    if let Some(storage) = storage {
        let log = Arc::new(SecurityEventLog::new());
        builder = builder
            .security_log_persisted(
                Arc::clone(&log),
                SecurityLogStore::new(storage.clone()),
                clock.clone(),
            )
            .await;
        security_log = Some(log);
    }

    builder = builder
        .remote_control(csms)
        .await
        .trigger_message(csms)
        .await
        .availability_control(csms)
        .await
        .reset(csms)
        .await;

    if let Some(storage) = storage {
        builder = builder.device_model_persistence(storage.clone()).await;
    }
    builder = builder.device_model(csms).await;
    // `has_csms`: reports periodic/aligned meter readings outward. Every meter reading a local
    // charger has is already on `ChargePointState::latest_meter_samples`, populated directly by
    // the hardware layer (H3b) and read back through `RunningCharger::apply_state` regardless of
    // this registration - so registering it locally would only spawn a loop that polls forever
    // (paced, not busy - `run_aligned_meter_values` backs off on a fixed interval when disabled)
    // to report to nobody. Skipped for the same "at best a no-op offline" reason as
    // `status_notifications` above, even though `docs/hardware-roadmap.md`'s H3c names "meter
    // values" among the locally-relevant blocks - the relevant local behavior (the meter itself
    // advancing) does not go through this registration at all.
    if has_csms {
        builder = builder
            .meter_values(csms, backoff.clone(), clock.clone())
            .await;
    }

    if capabilities.reservation {
        if let Some(storage) = storage {
            builder = builder
                .reservation_persistence(storage.clone(), clock.clone())
                .await;
        }
        builder = builder.reservation(csms).await.reservation_status_updates(
            csms,
            clock.clone(),
            backoff.clone(),
            60,
        );
    }
    if capabilities.local_auth_list {
        if let Some(storage) = storage {
            builder = builder
                .local_authorization_list_persistence(storage.clone())
                .await;
        }
        builder = builder.local_authorization_list(csms).await;
    }
    // `has_csms`: Tariff and Cost is purely CSMS-facing (a CSMS installing/reading tariffs, or
    // being told about accrued cost) with no locally-observable effect either way.
    if has_csms && capabilities.tariff_and_cost {
        builder = builder.cost(csms).await.tariffs(csms).await;
    }
    if capabilities.smart_charging {
        if let Some(storage) = storage {
            builder = builder
                .charging_profile_persistence(storage.clone(), clock.clone())
                .await;
        }
        builder = builder
            .smart_charging(
                csms,
                Arc::new(ChargingLimitProjection::new()),
                clock.clone(),
                backoff.clone(),
            )
            .await
            .charging_profile_reports(csms)
            .await;
    }
    // `has_csms`: Variable Monitoring's install/clear surface is CSMS-inbound only and its
    // reporting loops (`variable_monitor_events`) exist purely to notify a CSMS - nothing locally
    // observable depends on either.
    if has_csms && capabilities.variable_monitoring {
        builder = builder
            .variable_monitoring(csms)
            .await
            .monitoring_reports(csms)
            .await
            .variable_monitor_events(csms, backoff.clone(), clock.clone(), 60);
    }
    // `has_csms`: Periodic Event Stream exists only to push data to a CSMS on a schedule the CSMS
    // requested; with none, there is nothing to open a stream for.
    if has_csms && capabilities.periodic_event_stream {
        builder = builder
            .periodic_event_streams(csms, clock, backoff.clone(), 5)
            .await;
    }

    if let Some(display) = display {
        builder = builder.display_messages(csms, display).await;
    }

    (builder, security_log)
}

/// What every `Result`-returning method on [`NullCsms`] returns: there is no CSMS to answer, so
/// there is no answer - never "accepted", never "rejected", just unreachable. This is what a real
/// charger with no CSMS actually experiences (`docs/hardware-roadmap.md`'s "Known gaps"), and it
/// is the mechanism the local-authorization-list fix relies on: upstream's
/// `authorization::plain_decision` only ever consults the local authorization list and the
/// authorization cache from its `Err(_)` arm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NoCsms;

impl core::fmt::Display for NoCsms {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "no CSMS is connected - this is a local simulation")
    }
}

impl std::error::Error for NoCsms {}

/// The CSMS [`start_local_charger`](super::running_charger::start_local_charger) registers both
/// [`register_setup_blocks`] and [`register_optional_hardware`] against -
/// `docs/hardware-roadmap.md`'s H3c (functional blocks) and H3d (optional hardware), done as one
/// type because both are "local mode is under-wired" against the same shape of fix.
///
/// Implements the ~47-trait bound `register_setup_blocks`'s `N` requires, plus the six more
/// [`register_optional_hardware`] adds (`UpdateFirmwareHandler`, `SignedUpdateFirmwareHandler`,
/// `FirmwareStatusNotifier`, `GetLogHandler`, `LogStatusNotifier`, `CertificateHandler`) - the
/// whole reason local mode can route through both functions at all rather than a second
/// hand-built chain like `tests/smart_charging.rs` used to need (H8's gap). Modeled on this
/// module's own test-only `RecordingCsms` (same trait list, same shape) but answering every
/// question differently, on purpose:
///
/// - **Every `register_*_handler` method is a harmless no-op.** These only ever wire a callback
///   for a CSMS-initiated wire message (`UnlockConnector`, `Reset`, `SendLocalList`, ...) to this
///   charge point's actor; a local charger dials no CSMS, so no such message can ever arrive to
///   trigger one. Registering them costs nothing (a single non-blocking call) and completes the
///   functional-block shape, but nothing about local behavior depends on it.
/// - **Every method that would otherwise return a CSMS's decision, acknowledgement, or acceptance
///   returns `Err(NoCsms)`.** `authorize` is the fix this task exists for: an `Err` is exactly what
///   makes upstream fall back to the local authorization list and the authorization cache, instead
///   of the always-`Ok(Accepted)` `LocalAuthorizer` H3b registered, whose `Infallible` error type
///   made that fallback unreachable. The same honesty extends to `notify_boot` (never fabricates
///   registration acceptance - see [`register_setup_blocks`]'s `has_csms` doc comment for why
///   `provisioning`, the one block that would actually call this, is never registered at all),
///   `send_heartbeat`, and every `notify_*`/`send_*` method besides: none of them are wired up by
///   `register_setup_blocks` when `has_csms` is `false`, except `notify_reservation_status` (called
///   by `reservation_status_updates`'s expiry sweep, which stays registered locally because the
///   sweep itself - releasing an expired reservation - is real local behavior; the CSMS
///   notification about it failing is logged and otherwise harmless).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NullCsms;

#[async_trait::async_trait]
impl BootNotifier for NullCsms {
    type Error = NoCsms;
    async fn notify_boot(
        &self,
        _vendor_name: &str,
        _model_name: &str,
        _reason: Option<ocpp_charge_point::state::BootReasonCause>,
    ) -> Result<ocpp_charge_point::provisioning::BootNotificationOutcome, Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl HeartbeatSender for NullCsms {
    type Error = NoCsms;
    async fn send_heartbeat(&self) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl StatusNotifier for NullCsms {
    type Error = NoCsms;
    async fn notify_status(
        &self,
        _evse_id: usize,
        _connector_id: usize,
        _status: ocpp_charge_point::state::ConnectorStatus,
        _connector_state: ocpp_charge_point::state::ConnectorState,
    ) -> Result<(), Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl TransactionNotifier for NullCsms {
    type Error = NoCsms;
    async fn notify_transaction_event(
        &self,
        _evse_id: usize,
        _connector_id: usize,
        _kind: ocpp_charge_point::state::TransactionEventKind,
        _transaction: ocpp_charge_point::state::Transaction,
    ) -> Result<(), Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl Authorizer for NullCsms {
    type Error = NoCsms;
    async fn authorize(
        &self,
        _id_token: &ocpp_charge_point::state::IdToken,
    ) -> Result<ocpp_charge_point::state::AuthorizationStatus, Self::Error> {
        // The fix: an unreachable CSMS means no decision was ever made, which is what falls
        // upstream through to the local authorization list and the authorization cache. See this
        // type's doc comment.
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl UnlockConnectorHandler for NullCsms {
    async fn register_unlock_connector_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ChangeAvailabilityHandler for NullCsms {
    async fn register_change_availability_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl RequestStartTransactionHandler for NullCsms {
    async fn register_request_start_transaction_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl RequestStopTransactionHandler for NullCsms {
    async fn register_request_stop_transaction_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl TriggerMessageHandler for NullCsms {
    async fn register_trigger_message_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ReserveNowHandler for NullCsms {
    async fn register_reserve_now_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl CancelReservationHandler for NullCsms {
    async fn register_cancel_reservation_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ReservationStatusNotifier for NullCsms {
    type Error = NoCsms;
    async fn notify_reservation_status(
        &self,
        _update: ocpp_charge_point::state::ReservationUpdate,
    ) -> Result<(), Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl ResetHandler for NullCsms {
    async fn register_reset_handler(&self, _actor: ocpp_charge_point::actor::ChargePointActor) {}
}

#[async_trait::async_trait]
impl SendLocalListHandler for NullCsms {
    async fn register_send_local_list_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetLocalListVersionHandler for NullCsms {
    async fn register_get_local_list_version_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetVariablesHandler for NullCsms {
    async fn register_get_variables_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl SetVariablesHandler for NullCsms {
    async fn register_set_variables_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetBaseReportHandler for NullCsms {
    async fn register_get_base_report_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetReportHandler for NullCsms {
    async fn register_get_report_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl SecurityEventNotifier for NullCsms {
    type Error = NoCsms;
    async fn notify_security_event(
        &self,
        _event_type: &ocpp_charge_point::state::SecurityEventType,
        _tech_info: Option<&str>,
    ) -> Result<(), Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl CostUpdatedHandler for NullCsms {
    async fn register_cost_updated_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl SetDefaultTariffHandler for NullCsms {
    async fn register_set_default_tariff_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ChangeTransactionTariffHandler for NullCsms {
    async fn register_change_transaction_tariff_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ClearTariffsHandler for NullCsms {
    async fn register_clear_tariffs_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetTariffsHandler for NullCsms {
    async fn register_get_tariffs_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl MeterValuesNotifier for NullCsms {
    type Error = NoCsms;
    async fn send_meter_values(
        &self,
        _evse_id: usize,
        _connector_id: usize,
        _sample: ocpp_charge_point::state::MeterSample,
    ) -> Result<(), Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl SetChargingProfileHandler for NullCsms {
    async fn register_set_charging_profile_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ClearChargingProfileHandler for NullCsms {
    async fn register_clear_charging_profile_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetCompositeScheduleHandler for NullCsms {
    async fn register_get_composite_schedule_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
        _projection: Arc<ChargingLimitProjection>,
    ) {
    }
}

#[async_trait::async_trait]
impl GetChargingProfilesHandler for NullCsms {
    async fn register_get_charging_profiles_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl SetVariableMonitoringHandler for NullCsms {
    async fn register_set_variable_monitoring_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ClearVariableMonitoringHandler for NullCsms {
    async fn register_clear_variable_monitoring_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl VariableMonitorEventNotifier for NullCsms {
    type Error = NoCsms;
    async fn notify_variable_monitor_event(
        &self,
        _event: &ocpp_charge_point::state::TriggeredMonitor,
    ) -> Result<(), Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl SetMonitoringBaseHandler for NullCsms {
    async fn register_set_monitoring_base_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl SetMonitoringLevelHandler for NullCsms {
    async fn register_set_monitoring_level_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetMonitoringReportHandler for NullCsms {
    async fn register_get_monitoring_report_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl OpenPeriodicEventStreamHandler for NullCsms {
    async fn register_open_periodic_event_stream_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ClosePeriodicEventStreamHandler for NullCsms {
    async fn register_close_periodic_event_stream_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl AdjustPeriodicEventStreamHandler for NullCsms {
    async fn register_adjust_periodic_event_stream_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl GetPeriodicEventStreamHandler for NullCsms {
    async fn register_get_periodic_event_stream_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl PeriodicEventStreamNotifier for NullCsms {
    type Error = NoCsms;
    async fn notify_periodic_event_stream(
        &self,
        _sample: ocpp_charge_point::periodic_event_stream::PeriodicStreamSample,
    ) -> Result<(), Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl SetNetworkProfileHandler for NullCsms {
    async fn register_set_network_profile_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ClearCacheHandler for NullCsms {
    async fn register_clear_cache_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl SetDisplayMessageHandler for NullCsms {
    async fn register_set_display_message_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
        _supported_formats: Vec<ocpp_charge_point::state::MessageFormat>,
    ) {
    }
}

#[async_trait::async_trait]
impl GetDisplayMessagesHandler for NullCsms {
    async fn register_get_display_messages_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

#[async_trait::async_trait]
impl ClearDisplayMessageHandler for NullCsms {
    async fn register_clear_display_message_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
    ) {
    }
}

// --- `register_optional_hardware`'s six-trait bound (H3d) -------------------------------------
//
// Firmware/log-upload registration is itself has_csms-gated in `register_optional_hardware` (see
// its own doc comment), so none of these are ever actually *called* against `NullCsms` today -
// `certificates` is the one exception, and `register_certificate_handlers` below is a plain
// no-op like every other `register_*_handler` above. They exist so `NullCsms` satisfies the trait
// bound at all, the same reason the ~47 above do.

#[async_trait::async_trait]
impl UpdateFirmwareHandler for NullCsms {
    async fn register_update_firmware_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
        _updates: ocpp_charge_point::firmware::FirmwareUpdateQueue,
        _state: Arc<ocpp_charge_point::firmware::FirmwareUpdateState>,
    ) {
    }
}

#[async_trait::async_trait]
impl SignedUpdateFirmwareHandler for NullCsms {}

#[async_trait::async_trait]
impl FirmwareStatusNotifier for NullCsms {
    type Error = NoCsms;
    async fn notify_firmware_status(
        &self,
        _request_id: Option<i64>,
        _status: ocpp_charge_point::firmware::FirmwareStatus,
    ) -> Result<(), Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl GetLogHandler for NullCsms {
    async fn register_get_log_handler(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
        _uploads: ocpp_charge_point::diagnostics::LogUploadQueue,
        _state: Arc<ocpp_charge_point::diagnostics::LogUploadState>,
    ) {
    }
}

#[async_trait::async_trait]
impl LogStatusNotifier for NullCsms {
    type Error = NoCsms;
    async fn notify_log_status(
        &self,
        _request_id: Option<i64>,
        _status: ocpp_charge_point::diagnostics::LogUploadStatus,
    ) -> Result<(), Self::Error> {
        Err(NoCsms)
    }
}

#[async_trait::async_trait]
impl CertificateHandler for NullCsms {
    async fn register_certificate_handlers<S>(
        &self,
        _actor: ocpp_charge_point::actor::ChargePointActor,
        _store: S,
    ) where
        S: ocpp_charge_point::hardware::CertificateStore + Send + Sync + 'static,
    {
    }
}

#[async_trait::async_trait]
impl ReconnectHandler for NullCsms {
    async fn register_reconnect_handler<F, FF>(&self, _callback: F)
    where
        F: FnMut() -> FF + Send + Sync + 'static,
        FF: core::future::Future<Output = ()> + Send + 'static,
    {
        // Never invoked - a local charger never reconnects, since it never connected in the
        // first place - but harmless to drop either way.
    }
}

/// Dials `profile`'s CSMS as an OCPP 2.1 CSMS and runs the fake hardware built from `config`
/// (plus whatever `hardware` supplies - see [`ChargerHardware`]) against it. Only meaningful for
/// OCPP 2.1 chargers - callers must check `config.ocpp_version` themselves before calling this,
/// same as before; a CSMS that negotiates 1.6J or 2.0.1 over this dial gets
/// [`ConnectAndSetupError::UnsupportedNegotiatedVersion`] rather than a session, since driving
/// those versions' builder chains is not part of this crate's `charge_point_simulator_core` yet.
///
/// Built from [`ocpp_charge_point::ChargePointBuilder`] directly rather than
/// `ocpp_charge_point::connect_and_setup` - see `docs/hardware-roadmap.md`'s H2: `setup()` (which
/// `connect_and_setup` calls for a 2.1 session) returns an already-sealed
/// [`ChargePointRuntime`], with no way to register anything - including optional hardware like
/// `Storage`/`Display` - afterwards. Driving the builder ourselves keeps it open long enough to
/// add both OCPP 2.1's own extra blocks and `hardware`'s storage/display registrations (H5b/H6b) -
/// see [`register_setup_blocks`]'s doc comment for how those are gated.
pub async fn connect_charger(
    config: &ChargerConfig,
    profile: &ConnectionProfile,
    hardware: ChargerHardware,
) -> Result<RunningCharger, ConnectAndSetupError<Infallible>> {
    let ChargerHardware {
        storage,
        display,
        firmware_installer,
        firmware_verifier,
        file_transfer,
        certificate_store,
    } = hardware;

    let charge_point = FakeChargePoint::from_config(config);
    let url = websocket_url(&profile.csms_url, &profile.ocpp_identity);
    tracing::info!(
        charger = %config.id,
        identity = %profile.ocpp_identity,
        url = %url,
        "connecting to CSMS"
    );

    let SecurityProfile::Basic { password } = &profile.security;
    let options = ConnectOptions {
        username: Some(profile.ocpp_identity.as_str()),
        password: Some(password.as_str()),
        ..Default::default()
    };

    // Mirrors `ocpp_charge_point::connect_and_setup`: the redial target has to exist before the
    // connection does, because it *is* the connection's reconnector (installed into `options`
    // just below) - which version it redials as is filled in once the CSMS has picked one.
    let target = ConnectionTarget::new(&url, &options);
    target.set_max_inbound_frame_bytes(PayloadLimits::default().max_inbound_frame_bytes);

    let negotiated = ocpp_client::connect(&url, None, Some(target.install(options)))
        .await
        .map_err(|error| {
            tracing::warn!(charger = %config.id, %error, "the CSMS dial failed");
            ConnectAndSetupError::Connect(error)
        })?;

    match negotiated {
        NegotiatedClient::V2_1(client) => {
            target.set_version(OcppVersion::V2_1);
            connect_ocpp_2_1(
                charge_point,
                client,
                target,
                storage,
                display,
                firmware_installer,
                firmware_verifier,
                file_transfer,
                certificate_store,
            )
            .await
        }
        NegotiatedClient::V2_0_1(_) => Err(ConnectAndSetupError::UnsupportedNegotiatedVersion(
            OcppVersion::V2_0_1,
        )),
        NegotiatedClient::V1_6(_) => Err(ConnectAndSetupError::UnsupportedNegotiatedVersion(
            OcppVersion::V1_6,
        )),
    }
}

/// Runs a full OCPP 2.1 session: [`register_setup_blocks`] reproduces `setup()`'s registrations,
/// then this adds the 2.1-only extras `setup()` cannot register itself (it takes no redial
/// target, and priority charging/dynamic schedules don't exist on 2.0.1/1.6J) - mirroring
/// `ocpp_charge_point::connect::setup_ocpp_2_1`, whose source this is a faithful reproduction of:
///
/// - Priority charging (`register_use_priority_charging_handler` plus its
///   `NotifyPriorityCharging` worker) and dynamic charging profiles
///   (`register_update_dynamic_schedule_handler` plus the periodic pull loop), both only when the
///   hardware declared `smart_charging` - upstream registers these with the same two calls this
///   does, `ChargePointBuilder::priority_charging`/`dynamic_charging_profiles`, just inlined by
///   hand against a sealed runtime instead.
/// - `ConnectionTarget::attach_security_reporting` (F5.2: from here on, a redial's oversized
///   frame can report `MemoryExhaustion` on this charge point's own actor) and network-profile
///   switching (A9: move the connection when the CSMS changes the selected profile) - folded into
///   the one `ChargePointBuilder::network_profile_switching` call, which does both.
///
/// Not reproduced: the `WebSocketPingInterval` keepalive loop
/// (`ocpp_charge_point::keepalive::run_ping_interval_updates`) upstream also spawns here. It
/// takes a `ChargePointActor`, which `ocpp-charge-point` only ever hands out via
/// `ChargePointRuntime::actor()` - `pub(crate)` to that crate, and not reachable through
/// `ChargePointBuilder` either. A CSMS that writes `WebSocketPingInterval` on a simulated charger
/// therefore won't see the connection's keepalive cadence change until reconnect; everything else
/// this function registers is unaffected.
///
/// Also adds [`register_optional_hardware`] (H10b/H12b: firmware, file transfer and certificate
/// registrations) after the smart-charging extras above - see that function's own doc comment for
/// why it is a second call here rather than folded into [`register_setup_blocks`] itself.
#[allow(clippy::too_many_arguments)]
async fn connect_ocpp_2_1(
    charge_point: FakeChargePoint,
    client: ocpp_client::ocpp_2_1::OCPP2_1Client,
    target: Arc<ConnectionTarget>,
    storage: Option<FileStorage>,
    display: Option<FakeDisplay>,
    firmware_installer: Option<Arc<FakeFirmwareInstaller>>,
    firmware_verifier: Option<Arc<FakeFirmwareVerifier>>,
    file_transfer: Option<Arc<FakeFileTransfer>>,
    certificate_store: Option<FileCertificateStore>,
) -> Result<RunningCharger, ConnectAndSetupError<Infallible>> {
    // Cloned before `charge_point` is moved into `ChargePointBuilder::start` below - that call
    // wraps it in an `Arc` this function can never reach again (see `RunningCharger`'s doc
    // comment), so the only way to keep a handle for ticking the meter later is to have taken one
    // first. `firmware_installer`/`file_transfer` get the same treatment, for the same reason:
    // `register_optional_hardware` consumes its own copies, and `RunningCharger::tick` needs a
    // live handle to drive them once this function's own locals are gone - see `RunningCharger`'s
    // own doc comment for what it now advances.
    let hardware = charge_point.clone();
    let firmware_installer_handle = firmware_installer.clone();
    let file_transfer_handle = file_transfer.clone();
    let builder = ChargePointBuilder::start(charge_point, TokioExecutor)
        .await
        .map_err(ConnectAndSetupError::Start)?;

    let (mut builder, security_log) = register_setup_blocks(
        builder,
        &client,
        TokioBackoff,
        SystemMonotonicClock,
        SystemClock,
        storage.as_ref(),
        display,
        true, // has_csms: a real CSMS is dialed on this path - unchanged from before H3c.
    )
    .await;

    if builder.capabilities().smart_charging {
        builder = builder
            .priority_charging(&client)
            .await
            .dynamic_charging_profiles(&client, SystemClock, TokioBackoff, 30)
            .await;
    }

    builder = register_optional_hardware(
        builder,
        &client,
        TokioBackoff,
        SystemClock,
        firmware_installer,
        firmware_verifier,
        file_transfer,
        certificate_store,
        security_log, // decision 8: share whatever `register_setup_blocks` restored, if anything.
        true, // has_csms: a real CSMS is dialed on this path - see the function's own doc comment.
    )
    .await;

    builder = builder.network_profile_switching(&target, client.clone(), TokioBackoff);

    Ok(RunningCharger::new(
        builder.offline_queue_retries(TokioBackoff, 60).build(),
        hardware,
        firmware_installer_handle,
        file_transfer_handle,
    ))
}

/// Registers firmware, file transfer and certificate hardware (`docs/hardware-roadmap.md`'s H10b
/// (firmware/file transfer) and H12b (certificates)): [`ChargePointBuilder::firmware_updates`],
/// [`ChargePointBuilder::log_uploads`] and [`ChargePointBuilder::certificates`], each gated on the
/// matching [`Capabilities`](ocpp_charge_point::hardware::Capabilities) flag
/// (`firmware_management`, `diagnostics`, `certificate_management`) and on `hardware` actually
/// supplying the object it needs - the same "declared but not backed logs a warning and registers
/// nothing" contract [`register_setup_blocks`] uses for storage/display.
///
/// # Why this is not folded into `register_setup_blocks`
///
/// As of `docs/hardware-roadmap.md`'s H3d, both [`connect_ocpp_2_1`] and
/// [`super::running_charger::start_local_charger`] call this function too - the gap that used to
/// keep it separate (`start_local_charger` had no `ChargerHardware` to pass through) is closed. It
/// stays a second function anyway: its `N` bound (six traits: `UpdateFirmwareHandler`,
/// `SignedUpdateFirmwareHandler`, `FirmwareStatusNotifier`, `GetLogHandler`, `LogStatusNotifier`,
/// `CertificateHandler`) is disjoint from `register_setup_blocks`'s own ~47, and firmware/file
/// transfer/certificates are optional in a way the always-registered functional blocks aren't -
/// splitting them keeps each function's trait bound legible and lets
/// [`RecordingCsms`] drive either independently in tests. Both callers now run the *same*
/// registration sequence either way: `register_setup_blocks` then `register_optional_hardware`,
/// `has_csms` the only thing that differs between them.
///
/// # `has_csms`
///
/// `true` for [`connect_ocpp_2_1`], `false` for
/// [`super::running_charger::start_local_charger`] - the same split `register_setup_blocks` makes,
/// for the same reason:
///
/// - **`firmware_updates` (has_csms-gated):** a firmware *campaign* - `UpdateFirmware`/
///   `SignedUpdateFirmware` inbound, `FirmwareStatusNotification` outbound - is a CSMS round trip
///   from end to end. With no CSMS, nothing could ever send the request that starts one, and
///   `FirmwareStatusNotifier::notify_firmware_status` would have nobody to report to. Installing
///   firmware is itself local hardware behavior (see [`FakeFirmwareInstaller`]'s own docs, and the
///   roadmap task notes) - which is why the fakes remain fully constructible and tickable
///   (`RunningCharger::tick` reaches them - see its own doc comment) regardless of `has_csms` - but
///   the OCPP-level campaign this method registers is not.
/// - **`log_uploads` (has_csms-gated):** a log upload needs somewhere to upload to - the URL is
///   supplied by the CSMS's own `GetLog`/`GetDiagnostics` request. With no CSMS, no such request
///   can arrive, so registering it would spawn a background task that awaits an empty queue
///   forever - harmless (the queue's `recv` suspends rather than busy-loops or retries against
///   `has_csms`'s absence), but pointless, the same "at best a no-op offline" reasoning
///   `register_setup_blocks` gives `status_notifications`/`meter_values`.
/// - **`certificates` (ungated by has_csms):** `InstallCertificate`/`DeleteCertificate`/
///   `GetInstalledCertificateIds`/`CertificateSigned` are a single non-blocking
///   `register_certificate_handlers` call with no spawned loop - the same "cheap enough to keep
///   the functional-block shape complete even though nothing ever dials in to trigger it locally"
///   reasoning `register_setup_blocks` gives `clear_cache`/`remote_control`/... . Registering it
///   costs nothing regardless of whether a CSMS exists to ever call it - so it is the one block
///   `start_local_charger` actually registers through this function today.
///
/// # `security_log` (`docs/hardware-roadmap.md` decision 8)
///
/// The same [`Arc<SecurityEventLog>`](SecurityEventLog) [`register_setup_blocks`] returns - `Some`
/// when persistent storage backed it (restored from whatever survived the last restart), `None`
/// otherwise. `log_uploads` uses it directly when present, so an uploaded security log includes
/// history from before a restart rather than only what happened on this boot; when `None` (no
/// persistent storage declared), it falls back to a fresh, empty, in-RAM-only log - the same
/// behavior this function had before decision 8, and the correct one: there is no restored history
/// to share when nothing was ever persisted.
///
/// # Not registered here (and why)
///
/// - **`publish_firmware`** needs a `hardware::FirmwarePublisher`, which wave 3 (H10a) did not
///   build - only `FirmwareInstaller`/`FirmwareVerifier`/`FileTransfer` fakes exist. Upstream ships
///   no usable implementation either, only `NoFirmwarePublisher` (always `Err`). Registering it
///   against `NoFirmwarePublisher` while `Capabilities::firmware_publishing` reads `true` would be
///   exactly the "a `true` with nothing behind it is worse than a `false`" mistake the roadmap's
///   working agreements warn against, so this is left unregistered - implementing a
///   `FirmwarePublisher` fake is its own task.
/// - **`ocsp_status`/`ocsp_chain_status`** need a `hardware::OcspChecker`. Neither
///   [`FileCertificateStore`] nor the upstream [`StoredCertificates`](ocpp_charge_point::hardware::StoredCertificates)
///   it wraps implements that trait - `OcspChecker` is deliberately not folded into
///   `CertificateStore` upstream (see `Capabilities::ocsp_checking`'s own doc comment: it needs an
///   outbound path to a third-party OCSP responder, which a certificate store does not give a
///   charge point). So `certificate_store` "making the OCSP methods reachable" never actually
///   happens with what this crate has today.
/// - **A `KeyStore`** is not a parameter of this function at all - see [`ChargerHardware`]'s doc
///   comment for why no field exists to pass one from.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn register_optional_hardware<T, X, N, B, K>(
    mut builder: ChargePointBuilder<T, X>,
    csms: &N,
    backoff: B,
    clock: K,
    firmware_installer: Option<Arc<FakeFirmwareInstaller>>,
    firmware_verifier: Option<Arc<FakeFirmwareVerifier>>,
    file_transfer: Option<Arc<FakeFileTransfer>>,
    certificate_store: Option<FileCertificateStore>,
    security_log: Option<Arc<SecurityEventLog>>,
    has_csms: bool,
) -> ChargePointBuilder<T, X>
where
    X: Executor,
    N: UpdateFirmwareHandler
        + SignedUpdateFirmwareHandler
        + FirmwareStatusNotifier
        + GetLogHandler
        + LogStatusNotifier
        + CertificateHandler
        + Clone
        + Send
        + Sync
        + 'static,
    B: Backoff + Clone + Send + Sync + 'static,
    K: Clock + Clone + Send + Sync + 'static,
{
    let capabilities = builder.capabilities();

    if has_csms && capabilities.firmware_management {
        match (&firmware_installer, &file_transfer) {
            (Some(installer), Some(transfer)) => {
                builder = if let Some(verifier) = &firmware_verifier {
                    builder
                        .firmware_updates(
                            csms,
                            Arc::clone(transfer),
                            Arc::clone(installer),
                            clock.clone(),
                            backoff.clone(),
                            Arc::clone(verifier),
                        )
                        .await
                } else {
                    // No verifier configured: `NoFirmwareVerifier` fails closed on a *signed*
                    // update (per its own docs, mirroring `NoFirmwareInstaller`) while an unsigned
                    // update is unaffected - see `run_one_update`'s own "an update carrying neither
                    // field is unsigned by OCPP's own design" branch. The sensible default for a
                    // charger that declared `firmware_management` but not signature checking.
                    builder
                        .firmware_updates(
                            csms,
                            Arc::clone(transfer),
                            Arc::clone(installer),
                            clock.clone(),
                            backoff.clone(),
                            NoFirmwareVerifier,
                        )
                        .await
                };
            }
            _ => {
                tracing::warn!(
                    "the charger declares firmware_management but ChargerHardware has no \
                     firmware_installer/file_transfer - firmware updates will not be registered"
                );
            }
        }
    }

    if has_csms && capabilities.diagnostics {
        if let Some(transfer) = &file_transfer {
            // Decision 8: share the restored handle when there is one, so an uploaded log
            // includes history from before a restart - see this function's own doc comment on
            // `security_log`. Falls back to a fresh, empty, in-RAM-only log exactly when
            // `register_setup_blocks` returned `None` (no persistent storage declared), matching
            // this function's own behavior before decision 8 for that case.
            let log = security_log
                .clone()
                .unwrap_or_else(|| Arc::new(SecurityEventLog::new()));
            builder = builder
                .log_uploads(csms, Arc::clone(transfer), log, backoff.clone())
                .await;
        } else {
            tracing::warn!(
                "the charger declares diagnostics but ChargerHardware has no file_transfer - log \
                 upload will not be registered"
            );
        }
    }

    if capabilities.certificate_management {
        if let Some(store) = certificate_store {
            builder = builder.certificates(csms, store).await;
        } else {
            tracing::warn!(
                "the charger declares certificate_management but ChargerHardware has no \
                 certificate_store - certificate management will not be registered"
            );
        }
    }

    builder
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::{CapabilitiesConfig, OcppVersion as SimOcppVersion};
    use ocpp_charge_point::actor::ChargePointActor;
    use ocpp_charge_point::diagnostics::{
        GetLogOutcome, LogUploadQueue, LogUploadRequest, LogUploadState, LogUploadStatus,
        handle_get_log, render_security_log,
    };
    use ocpp_charge_point::firmware::{
        FirmwareStatus, FirmwareUpdateQueue, FirmwareUpdateRequest, FirmwareUpdateState,
        handle_update_firmware,
    };
    use ocpp_charge_point::hardware::LogKind;
    use ocpp_charge_point::persistence::restore_security_log;
    use ocpp_charge_point::provisioning::BootNotificationOutcome;
    use ocpp_charge_point::state::{
        AuthorizationStatus, BootReasonCause, ChargePointEvent, ConnectorState, ConnectorStatus,
        IdToken, MeterSample, RegistrationStatus, ReservationUpdate, SecurityEvent,
        SecurityEventType, Transaction, TransactionEventKind, TriggeredMonitor,
    };
    use std::sync::Mutex;
    use std::time::Duration;

    #[tokio::test]
    #[ignore = "hits a real local CSMS, run manually with --ignored"]
    async fn can_connect_to_the_local_dev_csms() {
        let config = ChargerConfig {
            id: "sim-test".into(),
            ocpp_version: SimOcppVersion::V21,
            evses: vec![],
            has_display: false,
            capabilities: Default::default(),
        };
        let profile = ConnectionProfile {
            csms_url: std::env::var("OCPP_TEST_URL")
                .unwrap_or_else(|_| "http://localhost:8082/flowion/dev".into()),
            ocpp_identity: std::env::var("OCPP_TEST_IDENTITY")
                .unwrap_or_else(|_| "sim-test".into()),
            security: SecurityProfile::basic(
                std::env::var("OCPP_TEST_PASSWORD").unwrap_or_default(),
            )
            .unwrap(),
        };

        match connect_charger(&config, &profile, ChargerHardware::default()).await {
            Ok(_runtime) => println!("connected successfully"),
            Err(error) => panic!("connect failed: {error}"),
        }
    }

    #[test]
    fn converts_http_to_ws_and_appends_the_identity() {
        assert_eq!(
            websocket_url("http://localhost:8082/flowion/dev", "CP001"),
            "ws://localhost:8082/flowion/dev/CP001"
        );
    }

    #[test]
    fn converts_https_to_wss() {
        assert_eq!(
            websocket_url("https://csms.example.com/dev", "CP001"),
            "wss://csms.example.com/dev/CP001"
        );
    }

    #[test]
    fn leaves_an_already_websocket_scheme_untouched() {
        assert_eq!(
            websocket_url("ws://localhost:8082/flowion/dev", "CP001"),
            "ws://localhost:8082/flowion/dev/CP001"
        );
        assert_eq!(
            websocket_url("wss://localhost:8082/flowion/dev", "CP001"),
            "wss://localhost:8082/flowion/dev/CP001"
        );
    }

    #[test]
    fn does_not_produce_a_double_slash_when_the_base_url_has_a_trailing_slash() {
        assert_eq!(
            websocket_url("http://localhost:8082/flowion/dev/", "CP001"),
            "ws://localhost:8082/flowion/dev/CP001"
        );
    }

    // --- `register_setup_blocks` equivalence -------------------------------------------------
    //
    // The risk `docs/hardware-roadmap.md`'s H2 calls out: a registration dropped while hand-
    // porting `setup()`'s body onto the builder doesn't fail to compile, it just goes quiet - a
    // CSMS message that mysteriously never gets answered. `RecordingCsms` implements every trait
    // `setup()` itself requires (so it can run *both* `register_setup_blocks` and the real
    // `ocpp_charge_point::setup()` against the same fake CSMS, with no live network needed) and
    // records the name of every `register_*`/`notify_boot` call it receives.
    //
    // Two tests read that log:
    //   - `reproduction_matches_upstream_setup_for_an_all_false_capabilities_charger` compares
    //     the resulting `ChargePointState` shape against upstream's own `setup()` - what the task
    //     asked for directly.
    //   - `every_unconditional_block_registers_and_no_gated_block_fires_when_all_capabilities_are_false`
    //     asserts the call log itself, which is the more sensitive check: most of `setup()`'s
    //     registrations (`clear_cache`, `remote_control`, `trigger_message`, ...) only ever call a
    //     `register_*` method on the CSMS client - they never touch `ChargePointState` at all, so
    //     a dropped one would pass the state-shape comparison undetected. The call log catches it
    //     directly.
    //
    // Not covered by either test: the 2.1-only extras `connect_ocpp_2_1` adds on top of
    // `register_setup_blocks` (priority charging, dynamic charging profiles, network-profile
    // switching). None of them touch `ChargePointState` either, and unlike the blocks above they
    // need a live `OCPP2_1Client` to type-check against (their builder methods take the concrete
    // client, not a generic CSMS bound) - RecordingCsms can't stand in for one. Those are only
    // exercised by `can_connect_to_the_local_dev_csms` above, run manually against a real CSMS.

    /// What `register_update_firmware_handler` captures - see `RecordingCsms::firmware_update_handle`'s
    /// own doc comment. A named alias purely to keep clippy's `type_complexity` lint quiet; the
    /// three-tuple itself carries no meaning beyond "what `firmware_updates` handed the CSMS".
    type FirmwareUpdateHandle = (
        ChargePointActor,
        FirmwareUpdateQueue,
        Arc<FirmwareUpdateState>,
    );

    /// What `register_get_log_handler` captures - the diagnostics counterpart of
    /// `FirmwareUpdateHandle`, for the same reason: it lets a test feed a synthetic `GetLog`
    /// through the crate's own public `handle_get_log` into the *registered* `run_log_uploads`
    /// worker `log_uploads` spawned, rather than a hand-built substitute for it.
    type LogUploadHandle = (ChargePointActor, LogUploadQueue, Arc<LogUploadState>);

    #[derive(Clone, Default)]
    struct RecordingCsms {
        calls: Arc<Mutex<Vec<String>>>,
        // H10b: `firmware_updates` hands `register_update_firmware_handler` the
        // `FirmwareUpdateQueue`/`FirmwareUpdateState` `run_firmware_updates` (spawned inside that
        // same builder call) reads from - the only way anything outside `ocpp-charge-point` can
        // reach either is to capture them here when they arrive, so a test can later call the
        // crate's own public `handle_update_firmware` to feed a synthetic request into the queue
        // that `run_firmware_updates` is already awaiting, driving the *registered* path rather
        // than a hand-built substitute for it.
        firmware_update_handle: Arc<Mutex<Option<FirmwareUpdateHandle>>>,
        // H3d/decision 8: the same capture, for `register_get_log_handler` - lets a test drive a
        // `GetLog` through `log_uploads`'s registered worker to prove which `SecurityEventLog`
        // handle it actually uploads from.
        log_upload_handle: Arc<Mutex<Option<LogUploadHandle>>>,
    }

    impl RecordingCsms {
        fn new() -> Self {
            Self::default()
        }

        fn record(&self, name: impl Into<String>) {
            self.calls.lock().expect("lock poisoned").push(name.into());
        }

        fn called(&self, name: &str) -> bool {
            self.calls
                .lock()
                .expect("lock poisoned")
                .iter()
                .any(|call| call == name)
        }

        fn call_count(&self, name: &str) -> usize {
            self.calls
                .lock()
                .expect("lock poisoned")
                .iter()
                .filter(|call| call.as_str() == name)
                .count()
        }

        /// Takes the handle `register_update_firmware_handler` captured, if any - `None` if
        /// `firmware_updates` was never registered against this CSMS. See the field's own doc
        /// comment.
        fn take_firmware_update_handle(&self) -> Option<FirmwareUpdateHandle> {
            self.firmware_update_handle
                .lock()
                .expect("lock poisoned")
                .take()
        }

        /// Takes the handle `register_get_log_handler` captured, if any - `None` if `log_uploads`
        /// was never registered against this CSMS. See the field's own doc comment.
        fn take_log_upload_handle(&self) -> Option<LogUploadHandle> {
            self.log_upload_handle.lock().expect("lock poisoned").take()
        }
    }

    /// Maps every [`FirmwareStatus`] to a fixed, `&'static str` name `RecordingCsms::record` can
    /// log - keeps `RecordingCsms.calls` a flat list of names the way every other registration
    /// records itself, rather than adding a second, differently-shaped log just for firmware
    /// status transitions.
    fn firmware_status_name(status: FirmwareStatus) -> &'static str {
        match status {
            FirmwareStatus::Idle => "firmware_status_idle",
            FirmwareStatus::DownloadScheduled => "firmware_status_download_scheduled",
            FirmwareStatus::Downloading => "firmware_status_downloading",
            FirmwareStatus::Downloaded => "firmware_status_downloaded",
            FirmwareStatus::DownloadFailed => "firmware_status_download_failed",
            FirmwareStatus::InstallScheduled => "firmware_status_install_scheduled",
            FirmwareStatus::Installing => "firmware_status_installing",
            FirmwareStatus::Installed => "firmware_status_installed",
            FirmwareStatus::InstallationFailed => "firmware_status_installation_failed",
            FirmwareStatus::InstallRebooting => "firmware_status_install_rebooting",
            FirmwareStatus::SignatureVerified => "firmware_status_signature_verified",
            FirmwareStatus::InvalidSignature => "firmware_status_invalid_signature",
        }
    }

    #[async_trait::async_trait]
    impl UpdateFirmwareHandler for RecordingCsms {
        async fn register_update_firmware_handler(
            &self,
            actor: ChargePointActor,
            updates: FirmwareUpdateQueue,
            state: Arc<FirmwareUpdateState>,
        ) {
            self.record("update_firmware");
            *self.firmware_update_handle.lock().expect("lock poisoned") =
                Some((actor, updates, state));
        }
    }

    #[async_trait::async_trait]
    impl SignedUpdateFirmwareHandler for RecordingCsms {}

    #[async_trait::async_trait]
    impl FirmwareStatusNotifier for RecordingCsms {
        type Error = Infallible;
        async fn notify_firmware_status(
            &self,
            _request_id: Option<i64>,
            status: FirmwareStatus,
        ) -> Result<(), Self::Error> {
            self.record(firmware_status_name(status));
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl GetLogHandler for RecordingCsms {
        async fn register_get_log_handler(
            &self,
            actor: ChargePointActor,
            uploads: LogUploadQueue,
            state: Arc<LogUploadState>,
        ) {
            self.record("get_log");
            *self.log_upload_handle.lock().expect("lock poisoned") = Some((actor, uploads, state));
        }
    }

    #[async_trait::async_trait]
    impl LogStatusNotifier for RecordingCsms {
        type Error = Infallible;
        async fn notify_log_status(
            &self,
            _request_id: Option<i64>,
            _status: LogUploadStatus,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl CertificateHandler for RecordingCsms {
        async fn register_certificate_handlers<S>(&self, _actor: ChargePointActor, _store: S)
        where
            S: ocpp_charge_point::hardware::CertificateStore + Send + Sync + 'static,
        {
            self.record("certificate_handlers");
        }
    }

    #[async_trait::async_trait]
    impl BootNotifier for RecordingCsms {
        type Error = Infallible;
        async fn notify_boot(
            &self,
            _vendor_name: &str,
            _model_name: &str,
            _reason: Option<BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            self.record("boot");
            Ok(BootNotificationOutcome {
                status: RegistrationStatus::Accepted,
                interval_secs: 60,
                current_time: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for RecordingCsms {
        type Error = Infallible;
        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            Ok(None)
        }
    }

    #[async_trait::async_trait]
    impl StatusNotifier for RecordingCsms {
        type Error = Infallible;
        async fn notify_status(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _status: ConnectorStatus,
            _connector_state: ConnectorState,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TransactionNotifier for RecordingCsms {
        type Error = Infallible;
        async fn notify_transaction_event(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _kind: TransactionEventKind,
            _transaction: Transaction,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Authorizer for RecordingCsms {
        type Error = Infallible;
        async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
            Ok(AuthorizationStatus::Accepted)
        }
    }

    #[async_trait::async_trait]
    impl UnlockConnectorHandler for RecordingCsms {
        async fn register_unlock_connector_handler(&self, _actor: ChargePointActor) {
            self.record("unlock_connector");
        }
    }

    #[async_trait::async_trait]
    impl ChangeAvailabilityHandler for RecordingCsms {
        async fn register_change_availability_handler(&self, _actor: ChargePointActor) {
            self.record("change_availability");
        }
    }

    #[async_trait::async_trait]
    impl RequestStartTransactionHandler for RecordingCsms {
        async fn register_request_start_transaction_handler(&self, _actor: ChargePointActor) {
            self.record("request_start_transaction");
        }
    }

    #[async_trait::async_trait]
    impl RequestStopTransactionHandler for RecordingCsms {
        async fn register_request_stop_transaction_handler(&self, _actor: ChargePointActor) {
            self.record("request_stop_transaction");
        }
    }

    #[async_trait::async_trait]
    impl TriggerMessageHandler for RecordingCsms {
        async fn register_trigger_message_handler(&self, _actor: ChargePointActor) {
            self.record("trigger_message");
        }
    }

    #[async_trait::async_trait]
    impl ReserveNowHandler for RecordingCsms {
        async fn register_reserve_now_handler(&self, _actor: ChargePointActor) {
            self.record("reserve_now");
        }
    }

    #[async_trait::async_trait]
    impl CancelReservationHandler for RecordingCsms {
        async fn register_cancel_reservation_handler(&self, _actor: ChargePointActor) {
            self.record("cancel_reservation");
        }
    }

    #[async_trait::async_trait]
    impl ReservationStatusNotifier for RecordingCsms {
        type Error = Infallible;
        async fn notify_reservation_status(
            &self,
            _update: ReservationUpdate,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl ResetHandler for RecordingCsms {
        async fn register_reset_handler(&self, _actor: ChargePointActor) {
            self.record("reset");
        }
    }

    #[async_trait::async_trait]
    impl SendLocalListHandler for RecordingCsms {
        async fn register_send_local_list_handler(&self, _actor: ChargePointActor) {
            self.record("send_local_list");
        }
    }

    #[async_trait::async_trait]
    impl GetLocalListVersionHandler for RecordingCsms {
        async fn register_get_local_list_version_handler(&self, _actor: ChargePointActor) {
            self.record("get_local_list_version");
        }
    }

    #[async_trait::async_trait]
    impl GetVariablesHandler for RecordingCsms {
        async fn register_get_variables_handler(&self, _actor: ChargePointActor) {
            self.record("get_variables");
        }
    }

    #[async_trait::async_trait]
    impl SetVariablesHandler for RecordingCsms {
        async fn register_set_variables_handler(&self, _actor: ChargePointActor) {
            self.record("set_variables");
        }
    }

    #[async_trait::async_trait]
    impl GetBaseReportHandler for RecordingCsms {
        async fn register_get_base_report_handler(&self, _actor: ChargePointActor) {
            self.record("get_base_report");
        }
    }

    #[async_trait::async_trait]
    impl GetReportHandler for RecordingCsms {
        async fn register_get_report_handler(&self, _actor: ChargePointActor) {
            self.record("get_report");
        }
    }

    #[async_trait::async_trait]
    impl SecurityEventNotifier for RecordingCsms {
        type Error = Infallible;
        async fn notify_security_event(
            &self,
            _event_type: &SecurityEventType,
            _tech_info: Option<&str>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl CostUpdatedHandler for RecordingCsms {
        async fn register_cost_updated_handler(&self, _actor: ChargePointActor) {
            self.record("cost_updated");
        }
    }

    #[async_trait::async_trait]
    impl SetDefaultTariffHandler for RecordingCsms {
        async fn register_set_default_tariff_handler(&self, _actor: ChargePointActor) {
            self.record("set_default_tariff");
        }
    }

    #[async_trait::async_trait]
    impl ChangeTransactionTariffHandler for RecordingCsms {
        async fn register_change_transaction_tariff_handler(&self, _actor: ChargePointActor) {
            self.record("change_transaction_tariff");
        }
    }

    #[async_trait::async_trait]
    impl ClearTariffsHandler for RecordingCsms {
        async fn register_clear_tariffs_handler(&self, _actor: ChargePointActor) {
            self.record("clear_tariffs");
        }
    }

    #[async_trait::async_trait]
    impl GetTariffsHandler for RecordingCsms {
        async fn register_get_tariffs_handler(&self, _actor: ChargePointActor) {
            self.record("get_tariffs");
        }
    }

    #[async_trait::async_trait]
    impl MeterValuesNotifier for RecordingCsms {
        type Error = Infallible;
        async fn send_meter_values(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _sample: MeterSample,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl SetChargingProfileHandler for RecordingCsms {
        async fn register_set_charging_profile_handler(&self, _actor: ChargePointActor) {
            self.record("set_charging_profile");
        }
    }

    #[async_trait::async_trait]
    impl ClearChargingProfileHandler for RecordingCsms {
        async fn register_clear_charging_profile_handler(&self, _actor: ChargePointActor) {
            self.record("clear_charging_profile");
        }
    }

    #[async_trait::async_trait]
    impl GetCompositeScheduleHandler for RecordingCsms {
        async fn register_get_composite_schedule_handler(
            &self,
            _actor: ChargePointActor,
            _projection: Arc<ChargingLimitProjection>,
        ) {
            self.record("get_composite_schedule");
        }
    }

    #[async_trait::async_trait]
    impl GetChargingProfilesHandler for RecordingCsms {
        async fn register_get_charging_profiles_handler(&self, _actor: ChargePointActor) {
            self.record("get_charging_profiles");
        }
    }

    #[async_trait::async_trait]
    impl SetVariableMonitoringHandler for RecordingCsms {
        async fn register_set_variable_monitoring_handler(&self, _actor: ChargePointActor) {
            self.record("set_variable_monitoring");
        }
    }

    #[async_trait::async_trait]
    impl ClearVariableMonitoringHandler for RecordingCsms {
        async fn register_clear_variable_monitoring_handler(&self, _actor: ChargePointActor) {
            self.record("clear_variable_monitoring");
        }
    }

    #[async_trait::async_trait]
    impl VariableMonitorEventNotifier for RecordingCsms {
        type Error = Infallible;
        async fn notify_variable_monitor_event(
            &self,
            _event: &TriggeredMonitor,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl SetMonitoringBaseHandler for RecordingCsms {
        async fn register_set_monitoring_base_handler(&self, _actor: ChargePointActor) {
            self.record("set_monitoring_base");
        }
    }

    #[async_trait::async_trait]
    impl SetMonitoringLevelHandler for RecordingCsms {
        async fn register_set_monitoring_level_handler(&self, _actor: ChargePointActor) {
            self.record("set_monitoring_level");
        }
    }

    #[async_trait::async_trait]
    impl GetMonitoringReportHandler for RecordingCsms {
        async fn register_get_monitoring_report_handler(&self, _actor: ChargePointActor) {
            self.record("get_monitoring_report");
        }
    }

    #[async_trait::async_trait]
    impl OpenPeriodicEventStreamHandler for RecordingCsms {
        async fn register_open_periodic_event_stream_handler(&self, _actor: ChargePointActor) {
            self.record("open_periodic_event_stream");
        }
    }

    #[async_trait::async_trait]
    impl ClosePeriodicEventStreamHandler for RecordingCsms {
        async fn register_close_periodic_event_stream_handler(&self, _actor: ChargePointActor) {
            self.record("close_periodic_event_stream");
        }
    }

    #[async_trait::async_trait]
    impl AdjustPeriodicEventStreamHandler for RecordingCsms {
        async fn register_adjust_periodic_event_stream_handler(&self, _actor: ChargePointActor) {
            self.record("adjust_periodic_event_stream");
        }
    }

    #[async_trait::async_trait]
    impl GetPeriodicEventStreamHandler for RecordingCsms {
        async fn register_get_periodic_event_stream_handler(&self, _actor: ChargePointActor) {
            self.record("get_periodic_event_stream");
        }
    }

    #[async_trait::async_trait]
    impl PeriodicEventStreamNotifier for RecordingCsms {
        type Error = Infallible;
        async fn notify_periodic_event_stream(
            &self,
            _sample: ocpp_charge_point::periodic_event_stream::PeriodicStreamSample,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl SetNetworkProfileHandler for RecordingCsms {
        async fn register_set_network_profile_handler(&self, _actor: ChargePointActor) {
            self.record("set_network_profile");
        }
    }

    #[async_trait::async_trait]
    impl ClearCacheHandler for RecordingCsms {
        async fn register_clear_cache_handler(&self, _actor: ChargePointActor) {
            self.record("clear_cache");
        }
    }

    #[async_trait::async_trait]
    impl SetDisplayMessageHandler for RecordingCsms {
        async fn register_set_display_message_handler(
            &self,
            _actor: ChargePointActor,
            _supported_formats: Vec<ocpp_charge_point::state::MessageFormat>,
        ) {
            self.record("set_display_message");
        }
    }

    #[async_trait::async_trait]
    impl GetDisplayMessagesHandler for RecordingCsms {
        async fn register_get_display_messages_handler(&self, _actor: ChargePointActor) {
            self.record("get_display_messages");
        }
    }

    #[async_trait::async_trait]
    impl ClearDisplayMessageHandler for RecordingCsms {
        async fn register_clear_display_message_handler(&self, _actor: ChargePointActor) {
            self.record("clear_display_message");
        }
    }

    #[async_trait::async_trait]
    impl ReconnectHandler for RecordingCsms {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: core::future::Future<Output = ()> + Send + 'static,
        {
            self.record("reconnect");
        }
    }

    // --- `RecordingStorage`: the persistence-side counterpart of `RecordingCsms` --------------
    //
    // None of the nine storage-backed registrations (`boot_reason_persistence`,
    // `transaction_persistence`, ...) ever call a `register_*` method on the CSMS - they only
    // touch storage, restoring on registration and persisting on every subsequent change. So
    // `RecordingCsms`'s call log can't see whether one fired at all, and for the three blocks
    // with a `_persisted` sibling (`status_notifications`/`transaction_events`/
    // `security_events`), it's the *only* way to prove the persisted form actually ran rather
    // than the plain form (both register exactly one `reconnect` handler - see
    // `register_setup_blocks`'s doc comment for why that count alone can't distinguish them).
    // `RecordingStorage` logs every `get` call's key - every restore calls `get` at least once,
    // even to find nothing there - so a key showing up here is direct proof its registration
    // executed.
    #[derive(Clone, Default)]
    struct RecordingStorage {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingStorage {
        fn new() -> Self {
            Self::default()
        }

        fn get_calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock poisoned").clone()
        }

        fn was_queried(&self, needle: &str) -> bool {
            self.get_calls().iter().any(|key| key.contains(needle))
        }

        fn query_count(&self, needle: &str) -> usize {
            self.get_calls()
                .iter()
                .filter(|key| key.contains(needle))
                .count()
        }
    }

    #[derive(Debug)]
    struct RecordingStorageError;

    impl std::fmt::Display for RecordingStorageError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "recording storage never actually fails")
        }
    }

    impl std::error::Error for RecordingStorageError {}

    #[async_trait::async_trait]
    impl Storage for RecordingStorage {
        type Error = RecordingStorageError;

        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
            self.calls
                .lock()
                .expect("lock poisoned")
                .push(key.to_string());
            Ok(None)
        }

        async fn set(&self, _key: &str, _value: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn remove(&self, _key: &str) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn all_false_capabilities_config() -> ChargerConfig {
        ChargerConfig {
            id: "builder-equivalence-test".into(),
            ocpp_version: SimOcppVersion::V21,
            evses: vec![],
            has_display: false,
            capabilities: Default::default(),
        }
    }

    #[tokio::test]
    async fn reproduction_matches_upstream_setup_for_an_all_false_capabilities_charger() {
        let config = all_false_capabilities_config();

        let our_builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        // Real hardware is supplied, but every capability is false - `register_setup_blocks` must
        // produce exactly the same session as if `storage`/`display` were `None`, proving hardware
        // presence alone never registers anything the charger didn't declare (H5b/H6b's gating
        // requirement).
        let (our_builder, _security_log) = register_setup_blocks(
            our_builder,
            &RecordingCsms::new(),
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
            Some(&RecordingStorage::new()),
            Some(FakeDisplay::new()),
            true, // has_csms: this test compares against a real (connected-path) setup() session.
        )
        .await;
        let our_runtime = our_builder.offline_queue_retries(TokioBackoff, 60).build();

        let upstream_runtime = ocpp_charge_point::setup(
            FakeChargePoint::from_config(&config),
            RecordingCsms::new(),
            TokioExecutor,
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
        )
        .await
        .expect("starting the fake hardware never fails");

        assert_eq!(
            our_runtime.state(),
            upstream_runtime.state(),
            "connect_charger's hand-driven builder chain produced a different ChargePointState \
             shape than ocpp_charge_point::setup() for the same all-false Capabilities - a \
             registration was dropped, reordered, or a gated block fired that shouldn't have"
        );
    }

    #[tokio::test]
    async fn every_unconditional_block_registers_and_no_gated_block_fires_when_all_capabilities_are_false()
     {
        let config = all_false_capabilities_config();
        let csms = RecordingCsms::new();
        let storage = RecordingStorage::new();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let (builder, _security_log) = register_setup_blocks(
            builder,
            &csms,
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
            Some(&storage),
            Some(FakeDisplay::new()),
            true, // has_csms: this test is specifically about the connected path's registrations.
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        // The 13 registrations `setup()` always makes. `status_notifications`/`transaction_events`/
        // `meter_values`/`authorization` don't call a synchronous `register_*` (they spawn a
        // forwarder loop instead - nothing to observe without a live event), so those four aren't
        // asserted here directly; `reconnect` below stands in for three of them.
        for block in [
            "boot",
            "clear_cache",
            "set_network_profile",
            "unlock_connector",
            "change_availability",
            "request_start_transaction",
            "request_stop_transaction",
            "trigger_message",
            "reset",
            "get_variables",
            "set_variables",
            "get_base_report",
            "get_report",
        ] {
            assert!(
                csms.called(block),
                "expected `{block}` to be registered by the unconditional setup blocks"
            );
        }
        // `provisioning`, `status_notifications`, `transaction_events` and `security_events` each
        // register a reconnect-flush handler - the one synchronous signal those four leave behind.
        assert_eq!(
            csms.call_count("reconnect"),
            4,
            "expected exactly the four reconnect-flush registrations setup()'s unconditional \
             blocks make (provisioning, status_notifications, transaction_events, security_events)"
        );

        // The 11 registrations gated on a `Capabilities` flag - none should fire while every flag
        // is false.
        for block in [
            "reserve_now",
            "cancel_reservation",
            "send_local_list",
            "get_local_list_version",
            "cost_updated",
            "set_default_tariff",
            "change_transaction_tariff",
            "clear_tariffs",
            "get_tariffs",
            "set_charging_profile",
            "clear_charging_profile",
            "get_composite_schedule",
            "get_charging_profiles",
            "set_variable_monitoring",
            "clear_variable_monitoring",
            "set_monitoring_base",
            "set_monitoring_level",
            "get_monitoring_report",
            "open_periodic_event_stream",
            "close_periodic_event_stream",
            "adjust_periodic_event_stream",
            "get_periodic_event_stream",
        ] {
            assert!(
                !csms.called(block),
                "expected `{block}` to stay unregistered when its capability is false"
            );
        }

        // H5b/H6b: real storage and display were supplied above, but nothing was declared, so
        // neither persistence nor display messages should have registered anything either.
        assert!(
            storage.get_calls().is_empty(),
            "expected no storage reads at all when has_persistent_storage is false, got {:?}",
            storage.get_calls()
        );
        for block in [
            "set_display_message",
            "get_display_messages",
            "clear_display_message",
        ] {
            assert!(
                !csms.called(block),
                "expected `{block}` to stay unregistered when has_display is false"
            );
        }
    }

    /// A charger declaring `has_persistent_storage` must additionally register every
    /// storage-backed block: the six independent ones plus the two gated behind their own
    /// functional-block capability - but neither of those two here, since `reservation`/
    /// `local_auth_list`/`smart_charging` are still false. `security_log_persisted` and the three
    /// dual-form blocks (`status_notifications`/`transaction_events`/`security_events`, each
    /// switched to their `_persisted` sibling) are the ones most at risk of the "silently
    /// no-ops" trap `register_setup_blocks`'s doc comment describes, since none of them are
    /// observable via `RecordingCsms` - only `RecordingStorage` can prove they actually ran.
    #[tokio::test]
    async fn a_charger_declaring_has_persistent_storage_registers_the_persistence_blocks() {
        let config = ChargerConfig {
            id: "storage-capability-test".into(),
            ocpp_version: SimOcppVersion::V21,
            evses: vec![],
            has_display: false,
            capabilities: CapabilitiesConfig {
                has_persistent_storage: true,
                ..Default::default()
            },
        };
        let storage = RecordingStorage::new();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let (builder, security_log) = register_setup_blocks(
            builder,
            &RecordingCsms::new(),
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
            Some(&storage),
            None::<FakeDisplay>,
            true, // has_csms: proving the connected path's persistence registrations.
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        assert!(
            security_log.is_some(),
            "expected a restored security log handle back (decision 8) when has_persistent_storage \
             is true"
        );

        for key in [
            "boot-reason",
            "txn",
            "auth-cache",
            "network-profiles",
            "device-model",
            "security-log",
            "queue/status",
            "queue/transaction",
            "queue/security",
        ] {
            assert!(
                storage.was_queried(key),
                "expected a storage read touching `{key}` when has_persistent_storage is true, \
                 got {:?}",
                storage.get_calls()
            );
        }
        // `reservation`/`local_auth_list`/`smart_charging` are still false, so their
        // persistence must not have run even though storage is available.
        for key in ["reservations", "local-auth-list", "charging-profiles"] {
            assert!(
                !storage.was_queried(key),
                "expected no storage read touching `{key}` while its own capability is false, \
                 got {:?}",
                storage.get_calls()
            );
        }
    }

    /// A charger declaring `has_display` must register the Display Message functional block -
    /// and, symmetrically with the storage test above, must not when it doesn't (already covered
    /// by `every_unconditional_block_registers...` above).
    #[tokio::test]
    async fn a_charger_declaring_has_display_registers_display_messages() {
        let config = ChargerConfig {
            id: "display-capability-test".into(),
            ocpp_version: SimOcppVersion::V21,
            evses: vec![],
            has_display: true,
            capabilities: Default::default(),
        };
        let csms = RecordingCsms::new();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let (builder, _security_log) = register_setup_blocks(
            builder,
            &csms,
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
            None::<&RecordingStorage>,
            Some(FakeDisplay::new()),
            true, // has_csms: proving the connected path's display-message registration.
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        for block in [
            "set_display_message",
            "get_display_messages",
            "clear_display_message",
        ] {
            assert!(
                csms.called(block),
                "expected `{block}` to be registered when has_display is true"
            );
        }
    }

    /// The trap `register_setup_blocks`'s doc comment calls out: `status_notifications`/
    /// `transaction_events`/`security_events` each have a `_persisted` sibling that shares the
    /// same single-use subscription with the plain form, so calling both would leave the second
    /// one silently doing nothing. Each of the three storage keys below must be read **exactly
    /// once** - not zero (the plain form ran and persistence was skipped) and not two-or-more (a
    /// bug called both forms, one of which no-op'd).
    #[tokio::test]
    async fn each_dual_form_block_persists_exactly_once_when_storage_is_available() {
        let config = ChargerConfig {
            id: "dual-form-test".into(),
            ocpp_version: SimOcppVersion::V21,
            evses: vec![],
            has_display: false,
            capabilities: CapabilitiesConfig {
                has_persistent_storage: true,
                ..Default::default()
            },
        };
        let csms = RecordingCsms::new();
        let storage = RecordingStorage::new();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let (builder, _security_log) = register_setup_blocks(
            builder,
            &csms,
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
            Some(&storage),
            None::<FakeDisplay>,
            true, // has_csms: proving the connected path's plain-vs-persisted dual-form choice.
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        for key in ["queue/status", "queue/transaction", "queue/security"] {
            assert_eq!(
                storage.query_count(key),
                1,
                "expected exactly one storage read touching `{key}`, got {:?}",
                storage.get_calls()
            );
        }
        // The plain/`_persisted` choice is mutually exclusive per block, but both forms register
        // exactly one reconnect-flush handler either way - this stays 4 regardless of which form
        // ran, and is not by itself proof persistence happened (see the storage assertions
        // above), just a sanity check that switching forms didn't also drop the reconnect wiring.
        assert_eq!(
            csms.call_count("reconnect"),
            4,
            "expected the same four reconnect-flush registrations regardless of whether the \
             plain or `_persisted` form of status/transaction/security notifications ran"
        );
    }

    // --- `register_optional_hardware` (H10b/H12b) ---------------------------------------------
    //
    // Mirrors the `register_setup_blocks` equivalence tests above: an all-false-capabilities
    // charger registers none of these blocks even with real hardware supplied, each capability
    // adds exactly its own block, and a missing-but-declared piece of hardware warns and skips
    // rather than panicking - the same contract `register_setup_blocks` gives storage/display.
    // `a_registered_firmware_install_progresses_and_completes_through_the_registered_path` below
    // is the reason this task exists at all: it proves a firmware install driven through this
    // registration function's `firmware_updates` call actually progresses and completes, not
    // merely that registration happened.

    use super::super::hardware::{FirmwareInstallStage, TransferProfile};

    fn optional_hardware_config(capabilities: CapabilitiesConfig) -> ChargerConfig {
        ChargerConfig {
            id: "optional-hardware-test".into(),
            ocpp_version: SimOcppVersion::V21,
            evses: vec![],
            has_display: false,
            capabilities,
        }
    }

    fn instant_file_transfer() -> Arc<FakeFileTransfer> {
        Arc::new(FakeFileTransfer::new(
            TransferProfile::instant(0),
            TransferProfile::instant(0),
        ))
    }

    fn certificate_store_over_temp_dir() -> (tempfile::TempDir, FileCertificateStore) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = FileCertificateStore::new(FileStorage::new(dir.path()));
        (dir, store)
    }

    #[tokio::test]
    async fn optional_hardware_registers_nothing_when_capabilities_are_false_even_with_hardware_present()
     {
        let config = optional_hardware_config(CapabilitiesConfig::default());
        let csms = RecordingCsms::new();
        let installer = Arc::new(FakeFirmwareInstaller::new(Duration::ZERO));
        let verifier = Arc::new(FakeFirmwareVerifier::new());
        let transfer = instant_file_transfer();
        let (_dir, store) = certificate_store_over_temp_dir();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let builder = register_optional_hardware(
            builder,
            &csms,
            TokioBackoff,
            SystemClock,
            Some(installer),
            Some(verifier),
            Some(transfer),
            Some(store),
            None, // security_log: not exercised by this test.
            true, // has_csms: proving hardware presence alone never registers anything undeclared.
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        for block in ["update_firmware", "get_log", "certificate_handlers"] {
            assert!(
                !csms.called(block),
                "expected `{block}` to stay unregistered when its capability is false, even with \
                 real hardware supplied"
            );
        }
    }

    #[tokio::test]
    async fn firmware_management_registers_firmware_updates_when_hardware_is_present() {
        let config = optional_hardware_config(CapabilitiesConfig {
            firmware_management: true,
            ..Default::default()
        });
        let csms = RecordingCsms::new();
        let installer = Arc::new(FakeFirmwareInstaller::new(Duration::ZERO));
        let transfer = instant_file_transfer();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let builder = register_optional_hardware(
            builder,
            &csms,
            TokioBackoff,
            SystemClock,
            Some(installer),
            None, // no verifier - `NoFirmwareVerifier` should stand in.
            Some(transfer),
            None,
            None, // security_log: not exercised by this test.
            true,
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        assert!(
            csms.called("update_firmware"),
            "expected firmware_updates to register an UpdateFirmware handler"
        );
        for block in ["get_log", "certificate_handlers"] {
            assert!(
                !csms.called(block),
                "expected `{block}` to stay unregistered - only firmware_management is declared"
            );
        }
    }

    #[tokio::test]
    async fn firmware_management_without_backing_hardware_registers_nothing() {
        let config = optional_hardware_config(CapabilitiesConfig {
            firmware_management: true,
            ..Default::default()
        });
        let csms = RecordingCsms::new();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let builder = register_optional_hardware(
            builder,
            &csms,
            TokioBackoff,
            SystemClock,
            None,
            None,
            None,
            None,
            None, // security_log: not exercised by this test.
            true,
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        assert!(
            !csms.called("update_firmware"),
            "expected no registration when firmware_management is declared but no \
             firmware_installer/file_transfer was supplied"
        );
    }

    #[tokio::test]
    async fn diagnostics_registers_log_uploads_when_file_transfer_is_present() {
        let config = optional_hardware_config(CapabilitiesConfig {
            diagnostics: true,
            ..Default::default()
        });
        let csms = RecordingCsms::new();
        let transfer = instant_file_transfer();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let builder = register_optional_hardware(
            builder,
            &csms,
            TokioBackoff,
            SystemClock,
            None,
            None,
            Some(transfer),
            None,
            None, // security_log: not exercised by this test - falls back to a fresh log.
            true,
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        assert!(
            csms.called("get_log"),
            "expected log_uploads to register a GetLog handler"
        );
        for block in ["update_firmware", "certificate_handlers"] {
            assert!(
                !csms.called(block),
                "expected `{block}` to stay unregistered - only diagnostics is declared"
            );
        }
    }

    #[tokio::test]
    async fn diagnostics_without_file_transfer_registers_nothing() {
        let config = optional_hardware_config(CapabilitiesConfig {
            diagnostics: true,
            ..Default::default()
        });
        let csms = RecordingCsms::new();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let builder = register_optional_hardware(
            builder,
            &csms,
            TokioBackoff,
            SystemClock,
            None,
            None,
            None,
            None,
            None, // security_log: not exercised by this test.
            true,
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        assert!(
            !csms.called("get_log"),
            "expected no registration when diagnostics is declared but no file_transfer was \
             supplied"
        );
    }

    #[tokio::test]
    async fn certificate_management_registers_certificates_when_store_is_present() {
        let config = optional_hardware_config(CapabilitiesConfig {
            certificate_management: true,
            ..Default::default()
        });
        let csms = RecordingCsms::new();
        let (_dir, store) = certificate_store_over_temp_dir();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let builder = register_optional_hardware(
            builder,
            &csms,
            TokioBackoff,
            SystemClock,
            None,
            None,
            None,
            Some(store),
            None, // security_log: not exercised by this test.
            true,
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        assert!(
            csms.called("certificate_handlers"),
            "expected certificates to register InstallCertificate/DeleteCertificate/... handlers"
        );
        for block in ["update_firmware", "get_log"] {
            assert!(
                !csms.called(block),
                "expected `{block}` to stay unregistered - only certificate_management is \
                 declared"
            );
        }
    }

    /// H3c's `has_csms` reasoning, applied to these three blocks (see
    /// `register_optional_hardware`'s own doc comment): `firmware_updates`/`log_uploads` are
    /// CSMS-driven and skip when there is no CSMS, while `certificates` is a single non-blocking
    /// registration that costs nothing to keep regardless - exactly like `clear_cache`/
    /// `remote_control`/... in `register_setup_blocks`. Not reachable from any call site today
    /// (`connect_ocpp_2_1` always passes `true`), but the behavior is real and worth locking down
    /// ahead of local mode gaining a `ChargerHardware` of its own.
    #[tokio::test]
    async fn has_csms_false_skips_firmware_and_diagnostics_but_registers_certificates_regardless() {
        let config = optional_hardware_config(CapabilitiesConfig {
            firmware_management: true,
            diagnostics: true,
            certificate_management: true,
            ..Default::default()
        });
        let csms = RecordingCsms::new();
        let installer = Arc::new(FakeFirmwareInstaller::new(Duration::ZERO));
        let transfer = instant_file_transfer();
        let (_dir, store) = certificate_store_over_temp_dir();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let builder = register_optional_hardware(
            builder,
            &csms,
            TokioBackoff,
            SystemClock,
            Some(installer),
            None,
            Some(transfer),
            Some(store),
            None,  // security_log: not exercised by this test.
            false, // has_csms: no CSMS is dialed.
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        for block in ["update_firmware", "get_log"] {
            assert!(
                !csms.called(block),
                "expected `{block}` to stay unregistered with no CSMS, even with capability and \
                 hardware both present"
            );
        }
        assert!(
            csms.called("certificate_handlers"),
            "expected certificates to register regardless of has_csms - a single non-blocking \
             call, the same reasoning register_setup_blocks gives clear_cache/remote_control/..."
        );
    }

    /// The reason H10b exists: proves a firmware install, once registered through
    /// `register_optional_hardware`, actually progresses through simulated time and completes via
    /// the *registered* path rather than a hand-built stand-in for it - `handle_update_firmware`
    /// (the same public entry point a real 1.6J/2.x wire adapter calls) feeds the queue
    /// `firmware_updates` spawned internally, `FakeFirmwareInstaller::tick` paces the install, and
    /// `RecordingCsms::notify_firmware_status` observes the CSMS-facing result.
    #[tokio::test]
    async fn a_registered_firmware_install_progresses_and_completes_through_the_registered_path() {
        let config = optional_hardware_config(CapabilitiesConfig {
            firmware_management: true,
            ..Default::default()
        });
        let csms = RecordingCsms::new();
        let installer = Arc::new(FakeFirmwareInstaller::new(Duration::from_secs(90)));
        let transfer = instant_file_transfer();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let builder = register_optional_hardware(
            builder,
            &csms,
            TokioBackoff,
            SystemClock,
            Some(Arc::clone(&installer)),
            None,
            Some(transfer),
            None,
            None, // security_log: not exercised by this test.
            true,
        )
        .await;
        let _runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        let (actor, updates, state) = csms
            .take_firmware_update_handle()
            .expect("register_update_firmware_handler must have captured a handle");

        // Feed a synthetic `UpdateFirmware` through the same public entry point a real wire
        // adapter calls - this drives the *registered* `run_firmware_updates` worker
        // `firmware_updates` spawned above, not a hand-built substitute for it.
        let outcome = handle_update_firmware(
            &actor,
            &updates,
            &state,
            FirmwareUpdateRequest {
                request_id: Some(1),
                location: "https://example.invalid/firmware.bin".into(),
                retrieve_at: None,
                install_at: None,
                signature: None,
                signing_certificate: None,
                retries: 0,
                retry_interval_secs: 30,
            },
        )
        .await;
        assert_eq!(
            outcome,
            ocpp_charge_point::firmware::UpdateFirmwareOutcome::Accepted
        );

        // The transfer is instant, so the worker reaches `Installing` as soon as it is scheduled -
        // wait for that rather than assuming a fixed number of yields.
        tokio::time::timeout(Duration::from_secs(5), async {
            while installer.stage() != FirmwareInstallStage::Installing {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the firmware install never reached Installing");

        installer.tick(Duration::from_secs(45));
        tokio::task::yield_now().await;
        assert_eq!(
            installer.stage(),
            FirmwareInstallStage::Installing,
            "half the required duration must not complete the install - this must be genuinely \
             paced by tick, not instantaneous"
        );

        installer.tick(Duration::from_secs(45));

        tokio::time::timeout(Duration::from_secs(5), async {
            while installer.stage() != FirmwareInstallStage::Installed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the firmware install never completed");

        assert!(
            csms.called(firmware_status_name(FirmwareStatus::Installing)),
            "expected a FirmwareStatusNotification reporting Installing"
        );
        assert!(
            csms.called(firmware_status_name(FirmwareStatus::Installed)),
            "expected a FirmwareStatusNotification reporting Installed"
        );
    }

    // --- H3d: `RunningCharger::tick` drives the whole bundle -----------------------------------

    /// The reason H3d's ticker unification exists: before it, `RunningCharger::tick` advanced
    /// only `FakeChargePoint`, so a registered firmware install never progressed unless the
    /// caller kept its own `Arc` clone of the installer and ticked it directly - which is what
    /// the test above does, and what H10b's own test had to do for lack of anywhere else to tick
    /// from. This proves the fix: once `RunningCharger` is holding the installer/file-transfer
    /// handles (the same ones `connect_ocpp_2_1`/`start_local_charger` now capture before handing
    /// their owned copies to `register_optional_hardware`), `running_charger.tick(..)` alone
    /// carries a CSMS-driven install all the way to completion. Nothing here ticks `installer`
    /// directly - it is moved into `RunningCharger::new` below and never touched again; every
    /// observation is made through `RecordingCsms`'s call log instead.
    #[tokio::test]
    async fn a_registered_firmware_install_progresses_to_completion_through_running_charger_tick_alone()
     {
        let config = optional_hardware_config(CapabilitiesConfig {
            firmware_management: true,
            ..Default::default()
        });
        let csms = RecordingCsms::new();
        let installer = Arc::new(FakeFirmwareInstaller::new(Duration::from_secs(90)));
        let transfer = instant_file_transfer();

        let charge_point = FakeChargePoint::from_config(&config);
        let handle = charge_point.clone();
        let builder = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .expect("starting the fake hardware never fails");
        let builder = register_optional_hardware(
            builder,
            &csms,
            TokioBackoff,
            SystemClock,
            Some(Arc::clone(&installer)),
            None,
            Some(Arc::clone(&transfer)),
            None,
            None,
            true, // has_csms: this is the connected path's shape - firmware_updates is has_csms-gated.
        )
        .await;
        let runtime = builder.offline_queue_retries(TokioBackoff, 60).build();

        // `installer`/`transfer` are moved in here - the same "clone before handing ownership
        // away" shape `connect_ocpp_2_1`/`start_local_charger` use, so this test's own locals go
        // out of scope with nothing left to tick by hand.
        let running_charger = RunningCharger::new(runtime, handle, Some(installer), Some(transfer));

        let (actor, updates, state) = csms
            .take_firmware_update_handle()
            .expect("register_update_firmware_handler must have captured a handle");
        let outcome = handle_update_firmware(
            &actor,
            &updates,
            &state,
            FirmwareUpdateRequest {
                request_id: Some(1),
                location: "https://example.invalid/firmware.bin".into(),
                retrieve_at: None,
                install_at: None,
                signature: None,
                signing_certificate: None,
                retries: 0,
                retry_interval_secs: 30,
            },
        )
        .await;
        assert_eq!(
            outcome,
            ocpp_charge_point::firmware::UpdateFirmwareOutcome::Accepted
        );

        // Ticks the *whole* charger repeatedly until the CSMS is told `Installed` - a tick issued
        // before the worker has reached `install()` is a documented no-op (see
        // `FakeFirmwareInstaller::tick`'s own doc comment), so this simply keeps advancing
        // simulated time until one lands after that point, rather than assuming an exact ordering
        // between the worker's own scheduling and this loop's ticks.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !csms.called(firmware_status_name(FirmwareStatus::Installed)) {
                running_charger.tick(Duration::from_secs(90)).await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the firmware install never completed through RunningCharger::tick alone");

        assert!(
            csms.called(firmware_status_name(FirmwareStatus::Installing)),
            "expected a FirmwareStatusNotification reporting Installing"
        );
    }

    // --- H3d: local mode's registration sequence -----------------------------------------------

    /// `start_local_charger` used to hardcode `None`/`None` for storage/display regardless of
    /// what `ChargerHardware` it was given, and never called `register_optional_hardware` at all -
    /// the same "local mode is under-wired" gap H3c had already fixed one layer up, for functional
    /// blocks. This proves the mechanism `start_local_charger` now goes through -
    /// `register_setup_blocks` with `has_csms: false` - still registers persistence and display
    /// when real hardware is present: `has_csms` alone never gates either off, so a local charger
    /// given a real bundle gets them, not a silent `None`.
    #[tokio::test]
    async fn has_csms_false_still_registers_persistence_and_display_when_real_hardware_is_present()
    {
        let config = ChargerConfig {
            id: "local-hardware-test".into(),
            ocpp_version: SimOcppVersion::V21,
            evses: vec![],
            has_display: true,
            capabilities: CapabilitiesConfig {
                has_persistent_storage: true,
                ..Default::default()
            },
        };
        let csms = RecordingCsms::new();
        let storage = RecordingStorage::new();

        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let (builder, security_log) = register_setup_blocks(
            builder,
            &csms,
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
            Some(&storage),
            Some(FakeDisplay::new()),
            false, // has_csms: exactly what start_local_charger now calls with real hardware.
        )
        .await;
        let _runtime = builder.build();

        assert!(
            storage.was_queried("security-log"),
            "expected persistence to register with real storage even though has_csms is false"
        );
        assert!(
            security_log.is_some(),
            "expected a restored security log handle back even in local mode"
        );
        for block in [
            "set_display_message",
            "get_display_messages",
            "clear_display_message",
        ] {
            assert!(
                csms.called(block),
                "expected `{block}` to register with a real display even though has_csms is false"
            );
        }
    }

    // --- decision 8: `log_uploads` shares the restored `SecurityEventLog` ----------------------

    /// Before decision 8, `register_optional_hardware`'s `log_uploads` call was handed a fresh,
    /// empty `Arc<SecurityEventLog>` rather than the one `register_setup_blocks`'s
    /// `security_log_persisted` restores into - so an uploaded security log silently omitted
    /// everything from before a restart, which rather defeats uploading it. This proves the fix
    /// end to end: an event recorded before a simulated restart survives into the log a *second*
    /// boot restores, and a `GetLog` driven through that second boot's *registered* `log_uploads`
    /// worker uploads bytes that include it.
    #[tokio::test]
    async fn an_uploaded_security_log_includes_entries_recorded_before_a_simulated_restart() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let config = ChargerConfig {
            id: "security-log-restart-test".into(),
            ocpp_version: SimOcppVersion::V21,
            evses: vec![],
            has_display: false,
            capabilities: CapabilitiesConfig {
                has_persistent_storage: true,
                diagnostics: true,
                ..Default::default()
            },
        };

        // --- before the restart: boot once, record a security event, and let it persist.
        let storage = FileStorage::new(dir.path());
        let builder =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let (builder, security_log) = register_setup_blocks(
            builder,
            &RecordingCsms::new(),
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
            Some(&storage),
            None::<FakeDisplay>,
            false, // has_csms: irrelevant to security_log_persisted, which isn't has_csms-gated.
        )
        .await;
        let security_log = security_log.expect("has_persistent_storage should back a security log");
        let runtime = builder.build();

        runtime
            .send(ChargePointEvent::SecurityEventOccurred(SecurityEvent {
                event_type: SecurityEventType::TamperDetectionActivated,
                tech_info: Some("door switch tripped".into()),
            }))
            .await
            .expect("sending to a freshly-built runtime never fails");

        // Waits for the event to actually reach *disk*, not merely `security_log`'s in-memory
        // copy: `run_security_log_persistence` records into the log synchronously but persists to
        // `storage` through a `spawn_blocking` write that can still be in flight when
        // `security_log.len()` already reads 2 - polling storage directly is what this test is
        // actually about (decision 8 is about what survives a restart).
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let scratch = SecurityEventLog::new();
                let recovered =
                    restore_security_log(&scratch, &SecurityLogStore::new(storage.clone())).await;
                if recovered >= 2 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the security event was never persisted to disk");
        assert!(
            security_log.len() >= 2,
            "expected the in-memory log to have the event too, not just storage"
        );
        drop(runtime);

        // --- the restart: a fresh boot over the same on-disk storage restores the log.
        let builder2 =
            ChargePointBuilder::start(FakeChargePoint::from_config(&config), TokioExecutor)
                .await
                .expect("starting the fake hardware never fails");
        let csms = RecordingCsms::new();
        let (builder2, security_log2) = register_setup_blocks(
            builder2,
            &csms,
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
            Some(&storage),
            None::<FakeDisplay>,
            false,
        )
        .await;
        let security_log2 = security_log2
            .expect("has_persistent_storage should back a security log after a restart");
        assert!(
            security_log2
                .entries()
                .iter()
                .any(|entry| entry.event.tech_info.as_deref() == Some("door switch tripped")),
            "expected the restored log to include the pre-restart event"
        );

        // --- the fix under test: `log_uploads` must receive *this* restored handle.
        let transfer = instant_file_transfer();
        let builder2 = register_optional_hardware(
            builder2,
            &csms,
            TokioBackoff,
            SystemClock,
            None,
            None,
            Some(Arc::clone(&transfer)),
            None,
            Some(Arc::clone(&security_log2)),
            true, // has_csms: log_uploads is has_csms-gated.
        )
        .await;
        let _runtime2 = builder2.offline_queue_retries(TokioBackoff, 60).build();

        let (actor, uploads, state) = csms
            .take_log_upload_handle()
            .expect("register_get_log_handler must have captured a handle");

        let outcome = handle_get_log(
            &actor,
            &uploads,
            &state,
            &SystemClock,
            LogUploadRequest {
                request_id: Some(1),
                log_kind: LogKind::Security,
                remote_location: "https://example.invalid/security.log".into(),
                oldest: None,
                latest: None,
                retries: 0,
                retry_interval_secs: 30,
            },
        )
        .await;
        assert!(
            matches!(outcome, GetLogOutcome::Accepted { .. }),
            "expected the GetLog request to be accepted, got {outcome:?}"
        );

        let uploaded = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(bytes) = transfer.last_upload() {
                    return bytes;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the log upload never ran");

        assert_eq!(
            uploaded,
            render_security_log(&security_log2.entries()),
            "expected the uploaded log to render the same entries the restored handle holds"
        );
        assert!(
            String::from_utf8_lossy(&uploaded).contains("door switch tripped"),
            "expected the uploaded log to include the event recorded before the simulated \
             restart, not just what happened since - see docs/hardware-roadmap.md decision 8"
        );
    }
}
