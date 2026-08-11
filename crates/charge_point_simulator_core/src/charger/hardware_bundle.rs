use std::path::PathBuf;
use std::sync::Arc;

use super::hardware::{
    FakeDisplay, FakeFileTransfer, FakeFirmwareInstaller, FakeFirmwareVerifier,
    FileCertificateStore, FileStorage,
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
/// # No `key_store` field
///
/// `docs/hardware-roadmap.md`'s H12 also names a `KeyStore`, but no field for one is added here.
/// [`super::hardware::FileKeyStore`] is generic over a `SoftwareCrypto` backend that upstream ships
/// as a trait with **no implementation** (`ring` is present transitively via the websocket TLS
/// stack, which would make it the obvious candidate, but choosing a crypto backend is a security
/// decision, not a wiring one - see the roadmap's "Known gaps": "No `SoftwareCrypto` backend
/// ships"). There is also, independent of that gap, no `ChargePointBuilder` method that registers
/// a `KeyStore` at all yet - `Capabilities::key_storage`'s own upstream doc says as much ("No OCPP
/// messages at all yet"). A `key_store` field here would have nothing to be handed to.
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
}

impl ChargerHardware {
    /// Real hardware: a [`FileStorage`] rooted at `storage_dir` plus a [`FakeDisplay`]. `connect_charger`
    /// (via `docs/hardware-roadmap.md`'s H5b/H6b registration) only actually wires either one up
    /// for a charger whose YAML `capabilities:` block declares the matching flag
    /// (`has_persistent_storage`/`has_display`) - passing real hardware here doesn't by itself
    /// register anything the charger didn't declare.
    ///
    /// Every other field - `firmware_installer`/`firmware_verifier`/`file_transfer`/
    /// `certificate_store` - stays `None`: unlike storage/display, each of those needs a
    /// scenario-specific choice (an install duration, a transfer profile, a certificate directory
    /// limit) this constructor has no honest default for. A caller wanting them constructs
    /// `ChargerHardware` directly - the fields are `pub` for exactly this.
    pub fn new(storage_dir: impl Into<PathBuf>) -> Self {
        Self {
            storage: Some(FileStorage::new(storage_dir)),
            display: Some(FakeDisplay::new()),
            ..Default::default()
        }
    }
}
