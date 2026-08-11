use core::convert::Infallible;
use std::sync::Arc;

use ocpp_charge_point::ChargePointBuilder;
use ocpp_charge_point::ConnectAndSetupError;
use ocpp_charge_point::authorization::{Authorizer, ClearCacheHandler};
use ocpp_charge_point::availability::{ChangeAvailabilityHandler, StatusNotifier};
use ocpp_charge_point::clock::{Clock, MonotonicClock, SystemClock, SystemMonotonicClock};
use ocpp_charge_point::connection::ReconnectHandler;
use ocpp_charge_point::cost::CostUpdatedHandler;
use ocpp_charge_point::device_model::{GetVariablesHandler, SetVariablesHandler};
use ocpp_charge_point::display_message::{
    ClearDisplayMessageHandler, GetDisplayMessagesHandler, SetDisplayMessageHandler,
};
use ocpp_charge_point::executor::{Executor, TokioExecutor};
use ocpp_charge_point::hardware::{ChargePoint, Connector, Display, Evse, Storage};
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
use super::hardware::{FakeChargePoint, FakeDisplay, FileStorage};
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
) -> ChargePointBuilder<T, X>
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
    if let Some(storage) = storage {
        builder = builder
            .security_log_persisted(
                Arc::new(SecurityEventLog::new()),
                SecurityLogStore::new(storage.clone()),
                clock.clone(),
            )
            .await;
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

    builder
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

/// The CSMS [`start_local_charger`](super::running_charger::start_local_charger) registers
/// [`register_setup_blocks`] against - `docs/hardware-roadmap.md`'s H3c, done together with the
/// local-authorizer fix in "Known gaps" because both are "local mode is under-wired".
///
/// Implements the same ~47-trait bound `register_setup_blocks`'s `N` requires - the whole reason
/// local mode can route through that function at all rather than a second hand-built chain like
/// `tests/smart_charging.rs` used to need (H8's gap). Modeled on this module's own test-only
/// `RecordingCsms` (same trait list, same shape) but answering every question differently, on
/// purpose:
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
    let ChargerHardware { storage, display } = hardware;

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
            connect_ocpp_2_1(charge_point, client, target, storage, display).await
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
async fn connect_ocpp_2_1(
    charge_point: FakeChargePoint,
    client: ocpp_client::ocpp_2_1::OCPP2_1Client,
    target: Arc<ConnectionTarget>,
    storage: Option<FileStorage>,
    display: Option<FakeDisplay>,
) -> Result<RunningCharger, ConnectAndSetupError<Infallible>> {
    // Cloned before `charge_point` is moved into `ChargePointBuilder::start` below - that call
    // wraps it in an `Arc` this function can never reach again (see `RunningCharger`'s doc
    // comment), so the only way to keep a handle for ticking the meter later is to have taken one
    // first.
    let hardware = charge_point.clone();
    let builder = ChargePointBuilder::start(charge_point, TokioExecutor)
        .await
        .map_err(ConnectAndSetupError::Start)?;

    let mut builder = register_setup_blocks(
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
    builder = builder.network_profile_switching(&target, client.clone(), TokioBackoff);

    Ok(RunningCharger::new(
        builder.offline_queue_retries(TokioBackoff, 60).build(),
        hardware,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::{CapabilitiesConfig, OcppVersion as SimOcppVersion};
    use ocpp_charge_point::actor::ChargePointActor;
    use ocpp_charge_point::provisioning::BootNotificationOutcome;
    use ocpp_charge_point::state::{
        AuthorizationStatus, BootReasonCause, ConnectorState, ConnectorStatus, IdToken,
        MeterSample, RegistrationStatus, ReservationUpdate, SecurityEventType, Transaction,
        TransactionEventKind, TriggeredMonitor,
    };
    use std::sync::Mutex;

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

    #[derive(Clone, Default)]
    struct RecordingCsms {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl RecordingCsms {
        fn new() -> Self {
            Self::default()
        }

        fn record(&self, name: &'static str) {
            self.calls.lock().expect("lock poisoned").push(name);
        }

        fn called(&self, name: &'static str) -> bool {
            self.calls.lock().expect("lock poisoned").contains(&name)
        }

        fn call_count(&self, name: &str) -> usize {
            self.calls
                .lock()
                .expect("lock poisoned")
                .iter()
                .filter(|call| **call == name)
                .count()
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
        let our_builder = register_setup_blocks(
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
        let builder = register_setup_blocks(
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
        let builder = register_setup_blocks(
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
        let builder = register_setup_blocks(
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
        let builder = register_setup_blocks(
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
}
