//! Simulated firmware installation and signature verification - `docs/hardware-roadmap.md`'s H10.
//!
//! [`FakeFirmwareInstaller`] models the second half of an OCPP firmware campaign (the first half,
//! fetching the image, is [`super::file_transfer::FakeFileTransfer::download`]): it does not
//! resolve until a configured amount of simulated time has been supplied via [`Self::tick`] -
//! never a real `tokio::time::sleep` (decision 2 in the roadmap's "Decisions taken") - so a CSMS
//! developer watching `FirmwareStatusNotification` genuinely sees `Installing` for a while before
//! `Installed`, exactly as against real hardware, while a test can drive the same install to
//! completion in zero wall-clock time by ticking it directly.
//!
//! [`FakeFirmwareVerifier`] is the signature-check gate between download and install. Unlike
//! installing, checking a signature is not itself a process with duration - it resolves as soon as
//! called, with whatever outcome was configured ahead of time.
//!
//! Both fakes make failure a first-class, deliberate, reproducible action - never random, per the
//! roadmap's working agreements - so a CSMS's `InstallationFailed`/rejected-signature handling is
//! exercisable on demand: see [`FakeFirmwareInstaller::trigger_failure`],
//! [`FakeFirmwareVerifier::set_outcome`], and [`FakeFirmwareVerifier::fail_to_verify`].

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ocpp_charge_point::hardware::{
    FirmwareInstallOutcome, FirmwareInstaller, FirmwareVerificationOutcome, FirmwareVerifier,
};
use tokio::sync::watch;

/// Where a simulated firmware install currently is - purely observational. Nothing in
/// `ocpp-charge-point` reads this; it exists so a frontend or a test can show or assert on
/// progress without racing [`FakeFirmwareInstaller::install`]'s own `Result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FirmwareInstallStage {
    /// No installation is in flight, and none has ever completed on this installer.
    #[default]
    Idle,
    /// [`FirmwareInstaller::install`] is in flight, waiting for [`FakeFirmwareInstaller::tick`] to
    /// supply enough simulated time (or for [`FakeFirmwareInstaller::trigger_failure`]).
    Installing,
    /// The most recent installation completed successfully.
    Installed,
    /// The most recent installation failed - see [`FakeFirmwareInstaller::trigger_failure`].
    Failed,
}

/// Simulated firmware installer: [`FirmwareInstaller::install`] does not resolve until this
/// type's configured [`Duration`] has been supplied via [`Self::tick`], so a CSMS driving a real
/// firmware campaign sees genuine `Installing` time rather than an instant jump to `Installed`.
/// Paced entirely by injected simulated time; never a real timer (`docs/hardware-roadmap.md`
/// decision 2).
///
/// Only one installation is tracked at a time - [`Self::install`] is not meant to be called again
/// while a previous call is still in flight on the same instance, mirroring how a single charge
/// point can only be mid-update once.
#[derive(Debug)]
pub struct FakeFirmwareInstaller {
    duration: Duration,
    reboot_required: bool,
    fail: AtomicBool,
    stage: Mutex<FirmwareInstallStage>,
    /// The in-flight installation's progress channel, `None` whenever no [`Self::install`] call
    /// is currently awaiting one - mirrors [`super::charge_point::FakeChargePoint`]'s
    /// `Mutex<Option<HardwareEventSender>>` `events` field: [`Self::tick`] is a no-op if nothing
    /// is listening yet.
    progress: Mutex<Option<watch::Sender<Duration>>>,
}

impl FakeFirmwareInstaller {
    /// An installer that reports [`FirmwareInstallOutcome::Installed`] once `duration` of
    /// simulated time has been ticked into it. `Duration::ZERO` completes on the first check, with
    /// no tick required.
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            reboot_required: false,
            fail: AtomicBool::new(false),
            stage: Mutex::new(FirmwareInstallStage::Idle),
            progress: Mutex::new(None),
        }
    }

    /// Configures this installer to report [`FirmwareInstallOutcome::RebootRequired`] on success
    /// instead of the default [`FirmwareInstallOutcome::Installed`].
    #[must_use]
    pub fn with_reboot_required(mut self) -> Self {
        self.reboot_required = true;
        self
    }

    /// Where the most recent (or in-flight) installation is.
    pub fn stage(&self) -> FirmwareInstallStage {
        *self.stage.lock().expect("lock poisoned")
    }

    /// Advances the in-flight installation (if any) by `elapsed`. A no-op when no
    /// [`Self::install`] call is currently awaiting progress, the same "nothing to drive yet"
    /// stance [`super::charge_point::FakeChargePoint::tick`] takes before `start` has run.
    pub fn tick(&self, elapsed: Duration) {
        let sender = self.progress.lock().expect("lock poisoned").clone();
        if let Some(sender) = sender {
            let total = *sender.borrow() + elapsed;
            let _ = sender.send(total);
        }
    }

    /// Runs an installation without a CSMS campaign behind it - the "technician with a USB stick"
    /// path, and the only way to reach this hardware at all in local mode, where
    /// [`super::super::connect::register_optional_hardware`] never registers `firmware_updates`
    /// (there is no CSMS to report progress to - see its `has_csms` doc comment).
    ///
    /// An inherent wrapper around [`FirmwareInstaller::install`], the trait method upstream's own
    /// `run_firmware_updates` calls, so a frontend can drive an install without importing
    /// `ocpp_charge_point`'s traits or naming its [`FirmwareInstallOutcome`]. The outcome is
    /// observable through [`Self::stage`] either way, which is what a frontend renders.
    ///
    /// Paced by [`Self::tick`] exactly as a CSMS-driven install is, so this resolves only once
    /// enough simulated time has been supplied - a caller holding an `Arc` of this can therefore
    /// start one, keep ticking, and watch it progress.
    pub async fn run_install(&self) -> Result<(), FakeFirmwareInstallerError> {
        FirmwareInstaller::install(self).await.map(|_| ())
    }

    /// Makes the current (or next) [`Self::install`] call fail with
    /// [`FakeFirmwareInstallerError`] instead of completing - the deliberate, reproducible way to
    /// exercise a CSMS's `InstallationFailed` handling, per the roadmap's working agreement that a
    /// fake must never fail randomly. Wakes an installation already waiting on [`Self::tick`] so
    /// the failure surfaces immediately rather than waiting for the next tick.
    ///
    /// Not resettable: once armed, every subsequent install on this instance fails too. A frontend
    /// offering this should say so rather than calling it "fail the next one".
    pub fn trigger_failure(&self) {
        self.fail.store(true, Ordering::Relaxed);
        if let Some(sender) = self.progress.lock().expect("lock poisoned").clone() {
            let current = *sender.borrow();
            let _ = sender.send(current);
        }
    }
}

/// The error [`FakeFirmwareInstaller::install`] returns after
/// [`FakeFirmwareInstaller::trigger_failure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeFirmwareInstallerError;

impl std::fmt::Display for FakeFirmwareInstallerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("simulated firmware installation failed")
    }
}

impl std::error::Error for FakeFirmwareInstallerError {}

#[async_trait::async_trait]
impl FirmwareInstaller for FakeFirmwareInstaller {
    type Error = FakeFirmwareInstallerError;

    async fn install(&self) -> Result<FirmwareInstallOutcome, Self::Error> {
        let (sender, mut receiver) = watch::channel(Duration::ZERO);
        *self.progress.lock().expect("lock poisoned") = Some(sender);
        *self.stage.lock().expect("lock poisoned") = FirmwareInstallStage::Installing;
        tracing::info!(
            duration_ms = self.duration.as_millis() as u64,
            "firmware installation starting"
        );

        let outcome = loop {
            if self.fail.load(Ordering::Relaxed) {
                break Err(FakeFirmwareInstallerError);
            }
            if *receiver.borrow() >= self.duration {
                break Ok(if self.reboot_required {
                    FirmwareInstallOutcome::RebootRequired
                } else {
                    FirmwareInstallOutcome::Installed
                });
            }
            if receiver.changed().await.is_err() {
                // The sender lives in `self.progress`, held above for the duration of this call -
                // unreachable while this loop is still running, since `progress` is only cleared
                // below, after the loop has already broken.
                break Err(FakeFirmwareInstallerError);
            }
        };

        *self.progress.lock().expect("lock poisoned") = None;
        match &outcome {
            Ok(outcome) => {
                *self.stage.lock().expect("lock poisoned") = FirmwareInstallStage::Installed;
                tracing::info!(?outcome, "firmware installation completed");
            }
            Err(_) => {
                *self.stage.lock().expect("lock poisoned") = FirmwareInstallStage::Failed;
                tracing::warn!("firmware installation failed");
            }
        }
        outcome
    }
}

/// Simulated firmware signature verification. Resolves as soon as called - checking a signature is
/// not itself a process that takes measurable time, unlike installing - with whatever outcome was
/// configured. Defaults to [`FirmwareVerificationOutcome::Valid`], so a CSMS's happy-path campaign
/// succeeds without extra setup, until a test or scenario asks for something else.
#[derive(Debug)]
pub struct FakeFirmwareVerifier {
    outcome: Mutex<FirmwareVerificationOutcome>,
    fail: AtomicBool,
}

impl Default for FakeFirmwareVerifier {
    fn default() -> Self {
        Self {
            outcome: Mutex::new(FirmwareVerificationOutcome::Valid),
            fail: AtomicBool::new(false),
        }
    }
}

impl FakeFirmwareVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures the outcome the next (and every subsequent, until changed again)
    /// [`Self::verify`] call reports - the deliberate way to exercise a CSMS's handling of
    /// [`FirmwareVerificationOutcome::InvalidSignature`]/
    /// [`FirmwareVerificationOutcome::InvalidSigningCertificate`].
    pub fn set_outcome(&self, outcome: FirmwareVerificationOutcome) {
        *self.outcome.lock().expect("lock poisoned") = outcome;
    }

    /// Makes [`Self::verify`] return `Err` instead of any outcome at all - the "could not even be
    /// attempted" case [`FirmwareVerifier::verify`]'s own docs describe (no verifier configured,
    /// or an unsupported algorithm), distinct from
    /// [`FirmwareVerificationOutcome::InvalidSignature`].
    pub fn fail_to_verify(&self) {
        self.fail.store(true, Ordering::Relaxed);
    }
}

/// The error [`FakeFirmwareVerifier::verify`] returns after
/// [`FakeFirmwareVerifier::fail_to_verify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeFirmwareVerifierError;

impl std::fmt::Display for FakeFirmwareVerifierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("simulated firmware verification could not be attempted")
    }
}

impl std::error::Error for FakeFirmwareVerifierError {}

#[async_trait::async_trait]
impl FirmwareVerifier for FakeFirmwareVerifier {
    type Error = FakeFirmwareVerifierError;

    async fn verify(
        &self,
        signing_certificate: Option<&str>,
        signature: Option<&str>,
    ) -> Result<FirmwareVerificationOutcome, Self::Error> {
        if self.fail.load(Ordering::Relaxed) {
            tracing::warn!("firmware verification could not be attempted");
            return Err(FakeFirmwareVerifierError);
        }
        let outcome = *self.outcome.lock().expect("lock poisoned");
        tracing::info!(
            has_certificate = signing_certificate.is_some(),
            has_signature = signature.is_some(),
            ?outcome,
            "firmware signature verified"
        );
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The local, CSMS-free entry point a frontend drives - the same install `run_firmware_updates`
    /// would have driven, reachable without importing upstream's trait or naming its outcome.
    #[tokio::test]
    async fn a_locally_run_install_reports_its_stage_and_still_needs_ticks() {
        let installer = FakeFirmwareInstaller::new(Duration::from_secs(90));
        assert_eq!(installer.stage(), FirmwareInstallStage::Idle);

        let install = installer.run_install();
        let drive = async {
            tokio::task::yield_now().await;
            assert_eq!(
                installer.stage(),
                FirmwareInstallStage::Installing,
                "an install awaiting ticks is observably in flight"
            );
            installer.tick(Duration::from_secs(90));
        };
        let (result, ()) = tokio::join!(install, drive);

        result.unwrap();
        assert_eq!(installer.stage(), FirmwareInstallStage::Installed);
    }

    #[tokio::test]
    async fn arming_a_failure_fails_a_locally_run_install_too() {
        let installer = FakeFirmwareInstaller::new(Duration::ZERO);
        installer.trigger_failure();

        assert_eq!(
            installer.run_install().await,
            Err(FakeFirmwareInstallerError)
        );
        assert_eq!(installer.stage(), FirmwareInstallStage::Failed);
    }

    #[tokio::test]
    async fn install_progresses_through_installing_before_completing_exactly_once() {
        let installer = FakeFirmwareInstaller::new(Duration::from_secs(90));
        assert_eq!(installer.stage(), FirmwareInstallStage::Idle);

        let install = installer.install();
        let drive = async {
            // Let `install` register its progress channel and enter `Installing` before we tick.
            tokio::task::yield_now().await;
            assert_eq!(installer.stage(), FirmwareInstallStage::Installing);

            installer.tick(Duration::from_secs(45));
            tokio::task::yield_now().await;
            assert_eq!(
                installer.stage(),
                FirmwareInstallStage::Installing,
                "half the required duration must not complete the install"
            );

            installer.tick(Duration::from_secs(45));
        };

        let (outcome, ()) = tokio::join!(install, drive);

        assert_eq!(outcome, Ok(FirmwareInstallOutcome::Installed));
        assert_eq!(installer.stage(), FirmwareInstallStage::Installed);
    }

    #[tokio::test]
    async fn a_zero_duration_installer_completes_on_the_first_check() {
        let installer = FakeFirmwareInstaller::new(Duration::ZERO);

        let outcome = installer.install().await;

        assert_eq!(outcome, Ok(FirmwareInstallOutcome::Installed));
    }

    #[tokio::test]
    async fn ticking_after_completion_is_a_harmless_no_op() {
        let installer = FakeFirmwareInstaller::new(Duration::ZERO);
        installer.install().await.unwrap();

        // No install is in flight anymore - this must not panic or resurrect a finished one.
        installer.tick(Duration::from_secs(1));

        assert_eq!(installer.stage(), FirmwareInstallStage::Installed);
    }

    #[tokio::test]
    async fn with_reboot_required_reports_reboot_required_on_success() {
        let installer = FakeFirmwareInstaller::new(Duration::ZERO).with_reboot_required();

        let outcome = installer.install().await;

        assert_eq!(outcome, Ok(FirmwareInstallOutcome::RebootRequired));
    }

    #[tokio::test]
    async fn splitting_the_same_elapsed_time_across_many_small_ticks_matches_one_large_tick() {
        async fn run(
            installer: &FakeFirmwareInstaller,
            chunks: &[Duration],
        ) -> FirmwareInstallOutcome {
            let install = installer.install();
            let drive = async {
                tokio::task::yield_now().await;
                for &chunk in chunks {
                    installer.tick(chunk);
                }
            };
            let (outcome, ()) = tokio::join!(install, drive);
            outcome.unwrap()
        }

        let one_tick = FakeFirmwareInstaller::new(Duration::from_secs(90));
        let one_tick_outcome = run(&one_tick, &[Duration::from_secs(90)]).await;

        let many_ticks = FakeFirmwareInstaller::new(Duration::from_secs(90));
        let chunks = vec![Duration::from_millis(900); 100];
        let many_ticks_outcome = run(&many_ticks, &chunks).await;

        assert_eq!(one_tick_outcome, FirmwareInstallOutcome::Installed);
        assert_eq!(one_tick_outcome, many_ticks_outcome);
    }

    #[tokio::test]
    async fn trigger_failure_makes_a_pending_install_return_err_without_panicking() {
        let installer = FakeFirmwareInstaller::new(Duration::from_secs(90));

        let install = installer.install();
        let drive = async {
            tokio::task::yield_now().await;
            installer.trigger_failure();
        };
        let (outcome, ()) = tokio::join!(install, drive);

        assert_eq!(outcome, Err(FakeFirmwareInstallerError));
        assert_eq!(installer.stage(), FirmwareInstallStage::Failed);
    }

    #[tokio::test]
    async fn trigger_failure_set_before_install_starts_fails_immediately() {
        let installer = FakeFirmwareInstaller::new(Duration::from_secs(90));
        installer.trigger_failure();

        let outcome = installer.install().await;

        assert_eq!(outcome, Err(FakeFirmwareInstallerError));
    }

    #[tokio::test]
    async fn verifier_defaults_to_valid() {
        let verifier = FakeFirmwareVerifier::new();

        let outcome = verifier.verify(Some("cert"), Some("sig")).await;

        assert_eq!(outcome, Ok(FirmwareVerificationOutcome::Valid));
    }

    #[tokio::test]
    async fn verifier_reports_a_configured_invalid_signature() {
        let verifier = FakeFirmwareVerifier::new();
        verifier.set_outcome(FirmwareVerificationOutcome::InvalidSignature);

        let outcome = verifier.verify(Some("cert"), Some("bad-sig")).await;

        assert_eq!(outcome, Ok(FirmwareVerificationOutcome::InvalidSignature));
    }

    #[tokio::test]
    async fn verifier_reports_a_configured_invalid_signing_certificate() {
        let verifier = FakeFirmwareVerifier::new();
        verifier.set_outcome(FirmwareVerificationOutcome::InvalidSigningCertificate);

        let outcome = verifier.verify(Some("untrusted-cert"), Some("sig")).await;

        assert_eq!(
            outcome,
            Ok(FirmwareVerificationOutcome::InvalidSigningCertificate)
        );
    }

    #[tokio::test]
    async fn verifier_can_be_made_to_fail_verification_entirely() {
        let verifier = FakeFirmwareVerifier::new();
        verifier.fail_to_verify();

        let outcome = verifier.verify(None, None).await;

        assert_eq!(outcome, Err(FakeFirmwareVerifierError));
    }

    /// The registration this drives (H10's follow-up) will require `FirmwareInstaller`/
    /// `FirmwareVerifier` + `Send + Sync + 'static` - a compile-time check that both fakes satisfy
    /// those bounds now, so a regression here is a build failure rather than a surprise later.
    #[allow(dead_code)]
    fn assert_installer_bounds<T: FirmwareInstaller + Send + Sync + 'static>() {}
    #[allow(dead_code)]
    fn assert_verifier_bounds<T: FirmwareVerifier + Send + Sync + 'static>() {}
    #[allow(dead_code)]
    fn fakes_satisfy_the_builder_bounds() {
        assert_installer_bounds::<FakeFirmwareInstaller>();
        assert_verifier_bounds::<FakeFirmwareVerifier>();
    }
}
