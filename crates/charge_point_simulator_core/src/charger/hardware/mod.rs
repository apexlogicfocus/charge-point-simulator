mod charge_point;
mod connector;
mod display;
mod evse;

pub use charge_point::FakeChargePoint;
pub use connector::FakeConnector;
// Not yet re-exported past this module: `charger/mod.rs` wiring `FakeDisplay` up to
// `ChargePointBuilder::display_messages` is H6b (`docs/hardware-roadmap.md`), a later task that
// also owns `charger/mod.rs`'s own re-export line. Until then nothing outside `display.rs`'s own
// tests reaches this path, which is what `rustc` is (correctly) noticing.
#[allow(unused_imports)]
pub use display::FakeDisplay;
pub use evse::FakeEvse;
