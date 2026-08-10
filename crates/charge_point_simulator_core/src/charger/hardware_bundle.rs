/// Optional hardware a simulated charger can be built with, beyond the three base traits
/// (`ChargePoint`/`Evse`/`Connector`) [`super::hardware::FakeChargePoint`] always implements -
/// `Storage`, `Display`, firmware installers/verifiers, certificate/key stores, and so on.
///
/// Empty today, on purpose. None of those optional trait implementations exist yet - they land
/// with `docs/hardware-roadmap.md`'s H5b (storage) and H6b (display) and later tasks - so there
/// is nothing to bundle. What this type buys is the *shape* of the change: `connect_charger`
/// takes a `ChargerHardware` today, so wiring up the first optional trait later just adds a field
/// here and a registration call in `connect_charger`'s body. Neither is a breaking change to a
/// published API, because the simulator's storage/display will be our own concrete types rather
/// than caller-supplied generics - a new field with a sensible default doesn't change how
/// existing callers construct or use this struct. Adding `connect_charger`'s hardware parameter
/// after callers already exist would have been the breaking change; adding it now, before any
/// exist, costs nothing.
///
/// Construct with `ChargerHardware::default()` until the first optional hardware type lands.
#[derive(Debug, Default)]
pub struct ChargerHardware {}
