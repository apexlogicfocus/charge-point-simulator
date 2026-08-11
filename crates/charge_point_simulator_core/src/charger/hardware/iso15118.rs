//! A simulated [`Iso15118Controller`] - the vehicle-facing half of ISO 15118 plug and charge,
//! `docs/hardware-roadmap.md`'s H13.
//!
//! # The boundary this module simulates, and the one it does not cross
//!
//! Upstream's own module docs (`ocpp_charge_point::hardware::iso15118`) are explicit that a real
//! [`Iso15118Controller`] would speak ISO 15118 over PLC/SLAC to an actual vehicle - a physical
//! layer and protocol stack this crate has no way to simulate, because there is no vehicle on the
//! other end of a simulator run. [`FakeIso15118Controller`] does not attempt to: it never decodes an
//! EXI payload, never negotiates a real HLC session, and performs **no cryptography at all**. Every
//! `exi_request`/`exi_response` string it produces or accepts is an opaque placeholder - readable
//! for debugging, never parsed - exactly the "carried opaquely, never parsed" stance upstream's own
//! [`Iso15118CertificateRequest`]/[`Iso15118CertificateResult`] docs describe for the real wire
//! format. This is deliberate, not a shortcut: **all genuine cryptography in this crate lives in
//! [`super::crypto`]** (key generation, signing, verification, all real `ring` primitives). This
//! module and that one do not depend on each other. A real contract-certificate installation would
//! ultimately rest on certificate-chain verification (the CSMS's job, per OCPP) and a key held by a
//! [`super::keys::FileKeyStore`]; this module only tracks the *outcome* of that exchange as the
//! simulated vehicle would see it - "I now hold a contract certificate" or "I don't" - so a CSMS
//! developer can watch a plug-and-charge flow complete or fail without a real vehicle, real EXI
//! codec, or real PKI attached.
//!
//! # What this controller adds beyond the bare trait
//!
//! [`Iso15118Controller`] itself is thin - one method, [`Iso15118Controller::deliver_certificate_response`],
//! delivering the CSMS's answer back to the vehicle. Modeling "a vehicle's contract-certificate
//! exchange" (not just the one delivery call) means also modeling the request the vehicle makes in
//! the first place, so [`FakeIso15118Controller::request_certificate`] exists as additional API
//! beyond the trait, the same way [`super::firmware::FakeFirmwareInstaller`] exposes `tick`/
//! `trigger_failure` beyond what `FirmwareInstaller` requires.
//!
//! # Failure paths, and how each is triggered on purpose
//!
//! Per the roadmap's working agreements, nothing here fails randomly - every failure is reachable
//! by a specific, reproducible action:
//!
//! - **An installation rejected** - the CSMS answered `Failed` rather than `Accepted`. Since
//!   [`Iso15118CertificateResult`]'s fields are all `pub`, a caller drives this directly by
//!   constructing one with `status: Iso15118CertificateStatus::Failed` and delivering it - no hook
//!   needed, the same way `FakeFirmwareVerifier::set_outcome` lets a caller pick an exact outcome.
//!   See `a_failed_result_does_not_install_anything` below.
//! - **A certificate absent** - the starting state of every fresh controller, and also explicitly
//!   reachable at any time via [`FakeIso15118Controller::revoke_contract_certificate`], simulating a
//!   vehicle that loses or never obtained a contract certificate.
//! - **An unsupported action** - [`FakeIso15118Controller::request_certificate`] with
//!   [`Iso15118CertificateAction::Update`] fails when no certificate is currently installed: a
//!   vehicle cannot ask to renew a contract certificate it does not hold. This is state-driven and
//!   deterministic (the state is the trigger), not a random or probabilistic failure.
//! - **Delivery itself fails** - [`FakeIso15118Controller::trigger_delivery_failure`] latches
//!   [`Iso15118Controller::deliver_certificate_response`] to always return `Err`, modeling the "HLC
//!   session already timed out, or the vehicle unplugged" case the trait's own docs describe.
//!   Mirrors [`super::firmware::FakeFirmwareInstaller::trigger_failure`]: once triggered, it stays
//!   triggered, with no un-trigger - a fresh controller is how a test gets a working one back.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ocpp_charge_point::hardware::{
    Iso15118CertificateAction, Iso15118CertificateRequest, Iso15118CertificateResult,
    Iso15118CertificateStatus, Iso15118Controller,
};

/// The simulated vehicle's contract certificate, as installed by a successful
/// [`Iso15118Controller::deliver_certificate_response`] call - the EXI response byte content is
/// carried opaquely, exactly as [`Iso15118CertificateResult::exi_response`] itself is, never parsed
/// by this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractCertificate {
    /// The raw `CertificateInstallationRes` the vehicle received, Base64-encoded EXI in a real
    /// exchange - here, whatever opaque content the caller's [`Iso15118CertificateResult`] carried.
    pub exi_response: String,
    /// How many more contract certificates the vehicle could still request, if the CSMS reported
    /// one (2.1 only - see [`Iso15118CertificateResult::remaining_contracts`]).
    pub remaining_contracts: Option<i64>,
}

/// The error [`FakeIso15118Controller`]'s operations return - see the module docs for how each
/// variant is triggered on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeIso15118ControllerError {
    /// Delivering the CSMS's response to the vehicle's simulated HLC session failed - see
    /// [`FakeIso15118Controller::trigger_delivery_failure`].
    DeliveryFailed,
    /// [`FakeIso15118Controller::request_certificate`] was asked for
    /// [`Iso15118CertificateAction::Update`] while the simulated vehicle holds no contract
    /// certificate to update.
    UnsupportedAction,
}

impl std::fmt::Display for FakeIso15118ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeliveryFailed => {
                f.write_str("could not deliver the certificate response to the simulated vehicle")
            }
            Self::UnsupportedAction => f.write_str(
                "the simulated vehicle cannot request a certificate update with no certificate installed",
            ),
        }
    }
}

impl std::error::Error for FakeIso15118ControllerError {}

/// A simulated vehicle's ISO 15118 plug-and-charge session - see the module docs for the
/// crypto/simulation boundary and how every failure path is triggered.
#[derive(Debug, Default)]
pub struct FakeIso15118Controller {
    contract_certificate: Mutex<Option<ContractCertificate>>,
    fail_delivery: AtomicBool,
    request_sequence: AtomicU64,
}

impl FakeIso15118Controller {
    /// A fresh controller: no contract certificate installed, deliveries succeed until
    /// [`Self::trigger_delivery_failure`] says otherwise.
    pub fn new() -> Self {
        Self::default()
    }

    /// The simulated vehicle's currently installed contract certificate, if any - `None` from a
    /// fresh controller or after [`Self::revoke_contract_certificate`].
    pub fn contract_certificate(&self) -> Option<ContractCertificate> {
        self.contract_certificate
            .lock()
            .expect("lock poisoned")
            .clone()
    }

    /// Simulates the vehicle losing (or never having obtained) its contract certificate - the
    /// deliberate, reproducible way to put a controller back in the "certificate absent" state, per
    /// the module docs.
    pub fn revoke_contract_certificate(&self) {
        *self.contract_certificate.lock().expect("lock poisoned") = None;
        tracing::info!("simulated vehicle's contract certificate revoked");
    }

    /// Simulates the vehicle emitting a `Get15118EVCertificate`-triggering request over the HLC
    /// link. The `exi_request`/`iso15118_schema_version` fields are simulator placeholders, not real
    /// EXI - see the module docs.
    ///
    /// Fails with [`FakeIso15118ControllerError::UnsupportedAction`] for
    /// [`Iso15118CertificateAction::Update`] when no contract certificate is currently installed: a
    /// vehicle has nothing to renew. [`Iso15118CertificateAction::Install`] never fails this way -
    /// requesting a first certificate never needs an existing one.
    pub fn request_certificate(
        &self,
        action: Iso15118CertificateAction,
    ) -> Result<Iso15118CertificateRequest, FakeIso15118ControllerError> {
        if matches!(action, Iso15118CertificateAction::Update)
            && self.contract_certificate().is_none()
        {
            tracing::warn!(
                "simulated vehicle requested a contract certificate update with none installed"
            );
            return Err(FakeIso15118ControllerError::UnsupportedAction);
        }

        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            ?action,
            sequence,
            "simulated vehicle requesting a contract certificate"
        );
        Ok(Iso15118CertificateRequest {
            action,
            exi_request: format!("SIMULATED-EXI-REQUEST-{sequence}"),
            iso15118_schema_version: "urn:iso:15118:2:2013:MsgDef".to_string(),
            maximum_contract_certificate_chains: None,
            prioritized_emaids: None,
        })
    }

    /// Makes every current and future [`Iso15118Controller::deliver_certificate_response`] call fail
    /// with [`FakeIso15118ControllerError::DeliveryFailed`] instead of completing - the deliberate,
    /// reproducible way to exercise "the HLC session timed out" / "the vehicle unplugged" handling,
    /// per the roadmap's working agreement that a fake must never fail randomly. Mirrors
    /// [`super::firmware::FakeFirmwareInstaller::trigger_failure`]: latches permanently, with no
    /// un-trigger.
    pub fn trigger_delivery_failure(&self) {
        self.fail_delivery.store(true, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl Iso15118Controller for FakeIso15118Controller {
    type Error = FakeIso15118ControllerError;

    async fn deliver_certificate_response(
        &self,
        result: &Iso15118CertificateResult,
    ) -> Result<(), Self::Error> {
        if self.fail_delivery.load(Ordering::Relaxed) {
            tracing::warn!("simulated ISO 15118 certificate delivery failed");
            return Err(FakeIso15118ControllerError::DeliveryFailed);
        }

        match result.status {
            Iso15118CertificateStatus::Accepted => {
                *self.contract_certificate.lock().expect("lock poisoned") =
                    Some(ContractCertificate {
                        exi_response: result.exi_response.clone(),
                        remaining_contracts: result.remaining_contracts,
                    });
                tracing::info!(
                    remaining_contracts = ?result.remaining_contracts,
                    "contract certificate installed on the simulated vehicle"
                );
            }
            Iso15118CertificateStatus::Failed => {
                tracing::info!("simulated vehicle informed of a rejected certificate installation");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(exi_response: &str) -> Iso15118CertificateResult {
        Iso15118CertificateResult {
            status: Iso15118CertificateStatus::Accepted,
            exi_response: exi_response.to_string(),
            remaining_contracts: Some(2),
        }
    }

    fn failed() -> Iso15118CertificateResult {
        Iso15118CertificateResult {
            status: Iso15118CertificateStatus::Failed,
            exi_response: String::new(),
            remaining_contracts: None,
        }
    }

    #[tokio::test]
    async fn a_fresh_controller_has_no_contract_certificate() {
        let controller = FakeIso15118Controller::new();
        assert_eq!(controller.contract_certificate(), None);
    }

    #[tokio::test]
    async fn an_accepted_result_installs_a_retrievable_contract_certificate() {
        let controller = FakeIso15118Controller::new();

        controller
            .deliver_certificate_response(&accepted("cert-response"))
            .await
            .unwrap();

        let installed = controller.contract_certificate().unwrap();
        assert_eq!(installed.exi_response, "cert-response");
        assert_eq!(installed.remaining_contracts, Some(2));
    }

    #[tokio::test]
    async fn a_failed_result_does_not_install_anything() {
        let controller = FakeIso15118Controller::new();

        controller
            .deliver_certificate_response(&failed())
            .await
            .unwrap();

        assert_eq!(controller.contract_certificate(), None);
    }

    #[tokio::test]
    async fn a_failed_result_does_not_clear_a_previously_installed_certificate() {
        let controller = FakeIso15118Controller::new();
        controller
            .deliver_certificate_response(&accepted("cert-response"))
            .await
            .unwrap();

        // A later `Update` attempt the CSMS rejects must not erase the certificate the vehicle
        // already holds - `Iso15118Controller`'s own docs say delivery "does not change" a result
        // already obtained, but say nothing about erasing state from before it.
        controller
            .deliver_certificate_response(&failed())
            .await
            .unwrap();

        assert!(controller.contract_certificate().is_some());
    }

    #[tokio::test]
    async fn revoking_returns_the_controller_to_a_bare_vehicle() {
        let controller = FakeIso15118Controller::new();
        controller
            .deliver_certificate_response(&accepted("cert-response"))
            .await
            .unwrap();
        assert!(controller.contract_certificate().is_some());

        controller.revoke_contract_certificate();

        assert_eq!(controller.contract_certificate(), None);
    }

    #[tokio::test]
    async fn requesting_an_install_never_requires_an_existing_certificate() {
        let controller = FakeIso15118Controller::new();

        let request = controller
            .request_certificate(Iso15118CertificateAction::Install)
            .unwrap();

        assert_eq!(request.action, Iso15118CertificateAction::Install);
    }

    #[tokio::test]
    async fn requesting_an_update_with_no_certificate_installed_is_unsupported() {
        let controller = FakeIso15118Controller::new();

        let result = controller.request_certificate(Iso15118CertificateAction::Update);

        assert_eq!(result, Err(FakeIso15118ControllerError::UnsupportedAction));
    }

    #[tokio::test]
    async fn requesting_an_update_after_install_succeeds() {
        let controller = FakeIso15118Controller::new();
        controller
            .deliver_certificate_response(&accepted("cert-response"))
            .await
            .unwrap();

        let request = controller
            .request_certificate(Iso15118CertificateAction::Update)
            .unwrap();

        assert_eq!(request.action, Iso15118CertificateAction::Update);
    }

    #[tokio::test]
    async fn successive_requests_get_distinct_placeholder_payloads() {
        let controller = FakeIso15118Controller::new();

        let first = controller
            .request_certificate(Iso15118CertificateAction::Install)
            .unwrap();
        let second = controller
            .request_certificate(Iso15118CertificateAction::Install)
            .unwrap();

        assert_ne!(first.exi_request, second.exi_request);
    }

    #[tokio::test]
    async fn trigger_delivery_failure_makes_delivery_fail_and_leaves_state_unchanged() {
        let controller = FakeIso15118Controller::new();
        controller.trigger_delivery_failure();

        let result = controller
            .deliver_certificate_response(&accepted("cert-response"))
            .await;

        assert_eq!(result, Err(FakeIso15118ControllerError::DeliveryFailed));
        assert_eq!(controller.contract_certificate(), None);
    }

    #[tokio::test]
    async fn a_triggered_delivery_failure_does_not_clear_an_already_installed_certificate() {
        let controller = FakeIso15118Controller::new();
        controller
            .deliver_certificate_response(&accepted("cert-response"))
            .await
            .unwrap();

        controller.trigger_delivery_failure();
        let result = controller
            .deliver_certificate_response(&accepted("newer-response"))
            .await;

        assert_eq!(result, Err(FakeIso15118ControllerError::DeliveryFailed));
        // The stale (pre-failure) certificate is still what the vehicle holds - the newer response
        // never made it across, exactly as a real timed-out delivery would leave things.
        assert_eq!(
            controller.contract_certificate().unwrap().exi_response,
            "cert-response"
        );
    }

    /// The registration a future task drives will require `Iso15118Controller + Send + Sync +
    /// 'static` - a compile-time check that `FakeIso15118Controller` satisfies those bounds now,
    /// mirroring every other fake's own bound-check test in this module directory.
    #[allow(dead_code)]
    fn assert_satisfies_the_builder_bounds<T: Iso15118Controller + Send + Sync + 'static>() {}
    #[allow(dead_code)]
    fn fake_iso15118_controller_satisfies_the_builder_bounds() {
        assert_satisfies_the_builder_bounds::<FakeIso15118Controller>();
    }
}
