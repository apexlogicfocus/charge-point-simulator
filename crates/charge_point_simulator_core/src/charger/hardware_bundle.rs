use std::path::PathBuf;
use std::sync::Arc;

use super::hardware::{
    EcdsaCrypto, FakeDisplay, FakeFileTransfer, FakeFirmwareInstaller, FakeFirmwareVerifier,
    FileCertificateStore, FileKeyStore, FileStorage,
};

/// Optional hardware a simulated charger can be built with, beyond the three base traits
/// (`ChargePoint`/`Evse`/`Connector`) [`super::hardware::FakeChargePoint`] always implements -
/// `Storage`, `Display`, firmware installers/verifiers, file transfer, certificate/key stores, and
/// so on.
///
/// `storage`/`display` were the first two fields to land, per `docs/hardware-roadmap.md`'s H5b
/// (storage) and H6b (display); `firmware_installer`/`firmware_verifier`/`file_transfer`/
/// `certificate_store` are H10b/H12b's. Every field defaults to `None`, so
/// [`ChargerHardware::default`] stays exactly what it was before any wave existed: no persistence,
/// no display, no firmware/file-transfer/certificate hardware, behaviourally identical to the
/// field-less struct H2 introduced. A new field with a `None` default doesn't change how an
/// existing caller that only ever constructs `ChargerHardware::default()` behaves - the whole
/// reason this type exists ahead of any field is so adding one is never a breaking change to a
/// published API.
///
/// `storage`/`display`/`certificate_store` are `core`'s own concrete types (not caller-supplied
/// generics) - see [`FileStorage`]/[`FakeDisplay`]/[`FileCertificateStore`]'s docs.
/// `firmware_installer`/`firmware_verifier`/`file_transfer` are `Arc`-wrapped, not because
/// `connect_charger` needs more than one owner, but because a caller who wants to drive
/// [`FakeFirmwareInstaller::tick`]/[`FakeFileTransfer::tick`] after handing this struct's fields
/// away must keep its own clone of the `Arc` *before* constructing `ChargerHardware` - the same
/// "clone before handing ownership away" shape [`super::running_charger::RunningCharger`] uses for
/// [`super::hardware::FakeChargePoint`]. `file_transfer` in particular can back more than one
/// registration at once (a firmware download and a diagnostics log upload are independent
/// campaigns that can both be in flight - see [`FakeFileTransfer::tick`]'s own docs), which is the
/// second reason it needs to be shared rather than owned outright by whichever registration runs
/// first.
///
/// [`FileStorage::new`] takes an explicit directory rather than resolving one itself (it "stays
/// decoupled from `dirs`", per its own module docs); locating that directory is the caller's job,
/// the same way [`super::catalog::discover_configured_chargers`] and
/// [`super::connection_store::ConnectionStore::load`] take an explicit [`std::path::Path`] rather
/// than reaching for the `dirs` crate internally. [`Self::new`] follows that precedent: it takes
/// the directory rather than deriving one from a charger id itself, leaving `dirs` resolution (and
/// any environment-variable override, like the TUI's `FLOWION_STATE_DIR`) to the caller - see
/// `crates/charge_point_simulator_tui/src/app.rs`'s call site.
///
/// # `key_store`: carried, not registered (`docs/hardware-roadmap.md` H13b)
///
/// `docs/hardware-roadmap.md`'s H12 named a `KeyStore`, and decision 5's reversal (`ring` →
/// RustCrypto) means [`super::hardware::FileKeyStore`] finally has a real `SoftwareCrypto` backend
/// to be generic over: [`super::hardware::EcdsaCrypto`]. So, unlike the state this doc comment
/// used to describe, a field for one now exists.
///
/// **It is never consumed by [`super::connect::connect_charger`] or
/// [`super::running_charger::start_local_charger`]**, though - both destructure this struct with
/// `..` on this field - because **`ChargePointBuilder` still has no method that registers a
/// `KeyStore` at all** (grep `builder.rs` for `KeyStore`: nothing). Two module-level functions
/// consume one instead - [`ocpp_charge_point::mutual_tls::client_config`] and
/// [`ocpp_charge_point::certificate_renewal::run_certificate_renewal`]/`renew_certificate` - and
/// this task investigated wiring both in. Neither is reachable without contorting this crate's
/// design, for two independent reasons:
///
/// - **`mutual_tls::client_config`** builds a TLS `ClientConfig` for OCPP security profile 3
///   (mutual TLS), which [`super::connection::SecurityProfile`] does not model at all today (it has
///   exactly one variant, `Basic`). Wiring it in would mean adding a new public
///   `SecurityProfile::TlsMutualAuth` variant, threading a `ClientConfig` through
///   `connect_charger`'s `ConnectOptions`, sourcing a CA `RootCertStore`, and building the
///   `GeneratedKeyPair`/`CertificateUse::ChargingStation` association the function's own doc
///   comment says is deliberately *not* its job to track ("this module does not track that
///   association itself ... the caller's (or F3.2's) job"). Each of those is its own design
///   decision, not wiring - the same "a security decision, not a convenience one" line decision 5
///   draws for the crypto backend itself.
/// - **`run_certificate_renewal`/`renew_certificate`** need a
///   `CertificateSigningRequester<ClientErr>` implementor. The only three that exist upstream
///   (`Ocpp2_1CertificateHandler` and its 2.0.1/1.6 counterparts, in `certificates/ocpp_2_1.rs` /
///   `ocpp_2_0_1.rs` / `ocpp_1_6.rs`) live behind a **private** `mod ocpp_2_1;` (not `pub mod`) -
///   confirmed by grepping `lib.rs` and `certificates.rs` for any `pub use` that re-exports them:
///   there is none. Unlike [`ocpp_charge_point::certificates::CertificateHandler`], this trait is
///   documented as deliberately *not* given a bare-`OCPP2_1Client` convenience impl either ("this
///   method's `PendingSignRequests` must be the same instance a `CertificateSigned` handler was
///   registered against ... construct one handler instance and use it for both" - a handler
///   instance nothing outside the crate can construct). So no publicly constructible type
///   implements this trait for any protocol version in `ocpp-charge-point` 0.1.0; using it would
///   mean reimplementing the private handler's CSR-building/`SignCertificate`-round-trip/
///   `PendingSignRequests` bookkeeping from scratch in this crate - duplicating upstream internals
///   that aren't ours to maintain, not wiring what's there.
///
/// So the field exists purely to be **carried**: a caller who wants a live handle for their own
/// TLS or renewal wiring should keep an `Arc::clone` of it *before* handing `ChargerHardware` to
/// `connect_charger`/`start_local_charger` - the same "clone before handing ownership away" shape
/// `firmware_installer`/`file_transfer` already use, and for the same reason (this struct is
/// consumed whole by either constructor). See
/// `crates/charge_point_simulator_core/src/charger/running_charger.rs`'s
/// `a_local_charger_given_a_real_bundle_still_leaves_the_callers_key_store_handle_usable` test for
/// exactly that pattern proven end to end.
#[derive(Debug, Default)]
pub struct ChargerHardware {
    /// Persisted key/value storage - see [`FileStorage`]. `None` means no durable storage, the
    /// same as before this field existed.
    pub storage: Option<FileStorage>,
    /// A simulated display - see [`FakeDisplay`]. `None` means no display, the same as before
    /// this field existed.
    pub display: Option<FakeDisplay>,
    /// A simulated firmware installer - see [`FakeFirmwareInstaller`]. `None` means no firmware
    /// installation capability.
    pub firmware_installer: Option<Arc<FakeFirmwareInstaller>>,
    /// A simulated firmware signature verifier - see [`FakeFirmwareVerifier`]. `None` means signed
    /// updates are refused (`ocpp_charge_point::hardware::NoFirmwareVerifier`'s fail-closed
    /// default), while unsigned updates are unaffected - see
    /// [`super::connect::register_optional_hardware`]'s doc comment.
    pub firmware_verifier: Option<Arc<FakeFirmwareVerifier>>,
    /// A simulated file transfer, backing both a firmware download and a diagnostics log upload -
    /// see [`FakeFileTransfer`]. `None` means neither can move bytes anywhere.
    pub file_transfer: Option<Arc<FakeFileTransfer>>,
    /// A certificate store persisted through [`FileStorage`] - see [`FileCertificateStore`].
    /// `None` means no certificate management capability.
    pub certificate_store: Option<FileCertificateStore>,
    /// A key store over [`FileStorage`] and the [`EcdsaCrypto`] backend - see [`FileKeyStore`].
    /// `None` means no key store. **Carried but never registered** - see this struct's own doc
    /// comment for the full explanation of why, and how a caller reaches it anyway.
    pub key_store: Option<Arc<FileKeyStore<EcdsaCrypto>>>,
}

impl ChargerHardware {
    /// Real hardware: a [`FileStorage`] rooted at `storage_dir` plus a [`FakeDisplay`]. Both
    /// [`super::connect::connect_charger`] and [`super::running_charger::start_local_charger`]
    /// (via `docs/hardware-roadmap.md`'s H5b/H6b registration, and H3d/decision 6 for local mode)
    /// only actually wire either one up for a charger whose YAML `capabilities:` block declares
    /// the matching flag (`has_persistent_storage`/`has_display`) - passing real hardware here
    /// doesn't by itself register anything the charger didn't declare.
    ///
    /// Every other field - `firmware_installer`/`firmware_verifier`/`file_transfer`/
    /// `certificate_store`/`key_store` - stays `None`: unlike storage/display, each of those needs
    /// a scenario-specific choice (an install duration, a transfer profile, a certificate directory
    /// limit, a key-count limit) this constructor has no honest default for. A caller wanting them
    /// constructs `ChargerHardware` directly - the fields are `pub` for exactly this.
    pub fn new(storage_dir: impl Into<PathBuf>) -> Self {
        Self {
            storage: Some(FileStorage::new(storage_dir)),
            display: Some(FakeDisplay::new()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_carries_no_key_store() {
        assert!(ChargerHardware::default().key_store.is_none());
    }

    #[test]
    fn new_does_not_populate_a_key_store() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        assert!(ChargerHardware::new(dir.path()).key_store.is_none());
    }
}
