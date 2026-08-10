use std::path::PathBuf;

use super::hardware::{FakeDisplay, FileStorage};

/// Optional hardware a simulated charger can be built with, beyond the three base traits
/// (`ChargePoint`/`Evse`/`Connector`) [`super::hardware::FakeChargePoint`] always implements -
/// `Storage`, `Display`, firmware installers/verifiers, certificate/key stores, and so on.
///
/// `storage`/`display` are the first two fields to land, per `docs/hardware-roadmap.md`'s H5b
/// (storage) and H6b (display) - later tasks add more. Every field defaults to `None`, so
/// [`ChargerHardware::default`] stays exactly what it was before either wave existed: no
/// persistence, no display, behaviourally identical to the field-less struct H2 introduced. A new
/// field with a `None` default doesn't change how an existing caller that only ever constructs
/// `ChargerHardware::default()` behaves - the whole reason this type exists ahead of any field is
/// so adding one is never a breaking change to a published API.
///
/// `storage`/`display` are `core`'s own concrete types (not caller-supplied generics) - see
/// [`FileStorage`]/[`FakeDisplay`]'s docs. [`FileStorage::new`] takes an explicit directory rather
/// than resolving one itself (it "stays decoupled from `dirs`", per its own module docs); locating
/// that directory is the caller's job, the same way [`super::catalog::discover_configured_chargers`]
/// and [`super::connection_store::ConnectionStore::load`] take an explicit [`std::path::Path`]
/// rather than reaching for the `dirs` crate internally. [`Self::new`] follows that precedent: it
/// takes the directory rather than deriving one from a charger id itself, leaving `dirs`
/// resolution (and any environment-variable override, like the TUI's `FLOWION_STATE_DIR`) to the
/// caller - see `crates/charge_point_simulator_tui/src/app.rs`'s call site.
#[derive(Debug, Default)]
pub struct ChargerHardware {
    /// Persisted key/value storage - see [`FileStorage`]. `None` means no durable storage, the
    /// same as before this field existed.
    pub storage: Option<FileStorage>,
    /// A simulated display - see [`FakeDisplay`]. `None` means no display, the same as before
    /// this field existed.
    pub display: Option<FakeDisplay>,
}

impl ChargerHardware {
    /// Real hardware: a [`FileStorage`] rooted at `storage_dir` plus a [`FakeDisplay`]. `connect_charger`
    /// (via `docs/hardware-roadmap.md`'s H5b/H6b registration) only actually wires either one up
    /// for a charger whose YAML `capabilities:` block declares the matching flag
    /// (`has_persistent_storage`/`has_display`) - passing real hardware here doesn't by itself
    /// register anything the charger didn't declare.
    pub fn new(storage_dir: impl Into<PathBuf>) -> Self {
        Self {
            storage: Some(FileStorage::new(storage_dir)),
            display: Some(FakeDisplay::new()),
        }
    }
}
