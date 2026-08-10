use core::convert::Infallible;
use std::sync::Arc;

use ocpp_charge_point::ChargePointBuilder;
use ocpp_charge_point::ChargePointRuntime;
use ocpp_charge_point::ConnectAndSetupError;
use ocpp_charge_point::authorization::{Authorizer, ClearCacheHandler};
use ocpp_charge_point::availability::{ChangeAvailabilityHandler, StatusNotifier};
use ocpp_charge_point::clock::{Clock, MonotonicClock, SystemClock, SystemMonotonicClock};
use ocpp_charge_point::connection::ReconnectHandler;
use ocpp_charge_point::cost::CostUpdatedHandler;
use ocpp_charge_point::device_model::{GetVariablesHandler, SetVariablesHandler};
use ocpp_charge_point::executor::{Executor, TokioExecutor};
use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse};
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
use ocpp_charge_point::security::SecurityEventNotifier;
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
use super::hardware::FakeChargePoint;
use super::hardware_bundle::ChargerHardware;

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
/// variable_monitor_events; periodic_event_streams).
///
/// Unlike `setup()`, this stops short of `offline_queue_retries`/`build()`: the whole reason
/// `connect_charger` drives the builder by hand instead of calling `setup()` is that `setup()`
/// seals the builder into a [`ChargePointRuntime`] before returning, with no hook left to add a
/// registration afterwards. Returning the still-open builder is what lets `connect_charger` add
/// OCPP 2.1's extra blocks `setup()` cannot know about (see its caller), and lets future
/// [`ChargerHardware`] fields (H5b/H6b) add their own registrations before the builder is
/// finally sealed - without another breaking signature change.
///
/// Generic over the CSMS client `N`, exactly like `setup()` is, so this same function - not a
/// reimplementation of it - is what the equivalence test below runs against a fake CSMS.
async fn register_setup_blocks<T, E, C, N, X, B, M, K>(
    mut builder: ChargePointBuilder<T, X>,
    csms: &N,
    backoff: B,
    monotonic: M,
    clock: K,
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
        + Clone
        + Send
        + Sync
        + 'static,
    X: Executor,
    B: Backoff + Clone + Send + Sync + 'static,
    M: MonotonicClock + Clone + Send + Sync + 'static,
    K: Clock + Clone + Send + Sync + 'static,
{
    builder = builder
        .provisioning(csms, backoff.clone(), monotonic)
        .await
        .status_notifications(csms)
        .await
        .transaction_events(csms)
        .await
        .authorization(csms, clock.clone())
        .await
        .clear_cache(csms)
        .await
        .network_profiles(csms)
        .await
        .security_events(csms)
        .await
        .remote_control(csms)
        .await
        .trigger_message(csms)
        .await
        .availability_control(csms)
        .await
        .reset(csms)
        .await
        .device_model(csms)
        .await
        .meter_values(csms, backoff.clone(), clock.clone())
        .await;

    // C3.1 upstream: each of these blocks only registers when the hardware actually declared the
    // matching capability - an absent capability means the CSMS gets `NotImplemented` rather than
    // a handler backed by hardware that can't do the thing.
    let capabilities = builder.capabilities();
    if capabilities.reservation {
        builder = builder.reservation(csms).await.reservation_status_updates(
            csms,
            clock.clone(),
            backoff.clone(),
            60,
        );
    }
    if capabilities.local_auth_list {
        builder = builder.local_authorization_list(csms).await;
    }
    if capabilities.tariff_and_cost {
        builder = builder.cost(csms).await.tariffs(csms).await;
    }
    if capabilities.smart_charging {
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
    if capabilities.variable_monitoring {
        builder = builder
            .variable_monitoring(csms)
            .await
            .monitoring_reports(csms)
            .await
            .variable_monitor_events(csms, backoff.clone(), clock.clone(), 60);
    }
    if capabilities.periodic_event_stream {
        builder = builder
            .periodic_event_streams(csms, clock, backoff.clone(), 5)
            .await;
    }

    builder
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
/// add both OCPP 2.1's own extra blocks and, once `ChargerHardware` grows fields, the simulator's
/// optional hardware.
pub async fn connect_charger(
    config: &ChargerConfig,
    profile: &ConnectionProfile,
    hardware: ChargerHardware,
) -> Result<ChargePointRuntime<FakeChargePoint>, ConnectAndSetupError<Infallible>> {
    // No fields yet - see `ChargerHardware`'s docs for why it exists ahead of them anyway. Named
    // (rather than `_hardware`) so a future field lands here as a used binding, not a warning.
    let ChargerHardware {} = hardware;

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
            connect_ocpp_2_1(charge_point, client, target).await
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
) -> Result<ChargePointRuntime<FakeChargePoint>, ConnectAndSetupError<Infallible>> {
    let builder = ChargePointBuilder::start(charge_point, TokioExecutor)
        .await
        .map_err(ConnectAndSetupError::Start)?;

    let mut builder = register_setup_blocks(
        builder,
        &client,
        TokioBackoff,
        SystemMonotonicClock,
        SystemClock,
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

    Ok(builder.offline_queue_retries(TokioBackoff, 60).build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::OcppVersion as SimOcppVersion;
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
    impl ReconnectHandler for RecordingCsms {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: core::future::Future<Output = ()> + Send + 'static,
        {
            self.record("reconnect");
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
        let our_builder = register_setup_blocks(
            our_builder,
            &RecordingCsms::new(),
            TokioBackoff,
            SystemMonotonicClock,
            SystemClock,
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
    }
}
