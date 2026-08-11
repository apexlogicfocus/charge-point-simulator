# What `ocpp-charge-point` would need to unblock the simulator

Three requests against **`ocpp-charge-point` 0.1.0**, from **Flowion Charge Point Simulator**
(`charge_point_simulator_core`, a downstream consumer via crates.io). Each one is a capability the
simulator cannot provide *at all* today, not a convenience — and each is blocked on something being
`pub(crate)` or on a type having no variant for the thing being asked, never on a missing
implementation on our side.

Written to be handed to someone working in the `ocpp-charge-point` repo cold. Every claim below was
checked against the published 0.1.0 source (paths and line numbers are from that crate, not from
ours), and each request says what we would do with it, so the design can be argued about on the
merits rather than guessed at.

Ordered by how much they unblock. **Request 1 unblocks three separate features; request 2 is the only
one that needs a new enum variant; request 3 is the smallest.**

---

## Request 1 — a way to reach the actor, or builder hooks that use it for us

**What is blocked:** the WebSocket keepalive loop, any hardware watchdog, and oversized-frame
security reporting on a 1.6J session's redials. All three, for the same reason.

### The situation

`ChargePointRuntime::actor()` is `pub(crate)` (`src/runtime.rs:166`), and nothing on
`ChargePointBuilder` exposes an equivalent. Two public APIs need a `&ChargePointActor` and are
therefore unreachable from outside the crate:

- **`keepalive::run_ping_interval_updates(actor: &ChargePointActor, control: &P)`**
  (`src/keepalive.rs:85`) — `pub`, and takes the one type we cannot obtain. It applies a
  CSMS-written `WebSocketPingInterval` device-model variable to the live connection. Your own
  `connect_and_setup` spawns it internally, so an integration that goes through `connect_and_setup`
  gets it and one that drives `ChargePointBuilder` does not.
- **`ConnectionTarget::attach_security_reporting(runtime.actor())`** — the call upstream's own
  `setup_ocpp_1_6` ends with, so a redial that meets an oversized frame can report
  `MemoryExhaustion`. The 2.x paths get it folded into `network_profile_switching`, but 1.6J has no
  network-profile message, so upstream reaches for the actor directly — and a downstream 1.6J session
  therefore cannot report it at all.
- **`Watchdog`** (`src/hardware/watchdog.rs:46`) — the trait is `pub` and documented for
  integrators to implement ("a watchdog is a peripheral: feeding one is a register write on an MCU, a
  `/dev/watchdog` write under Linux"). It is fed from exactly one place, the actor's run loop, via
  `ChargePointActor::spawn_with_watchdog` (`src/actor/charge_point_actor.rs:133`). `grep -c watchdog
  src/builder.rs` is **0**, and `ChargePointRuntime::new`/`new_with_limits` (the constructors
  `ChargePointBuilder::start` uses, `src/builder.rs:259`) never take one. So the trait can be
  implemented and then never installed.

### Why this reached us

We build the charge point through `ChargePointBuilder` rather than `connect_and_setup`, because
`connect_and_setup` cannot register optional hardware (firmware installers, file transfer,
certificate stores) — it takes no parameters for them. Driving the builder ourselves is the only way
to register that hardware, and the price is losing the keepalive loop, permanently and silently. We
recorded it as "the one thing the builder path genuinely cannot match".

### What would unblock it

Either of these; the second is nicer for us, the first is smaller for you.

```rust
// Option A: make the existing accessor public. One-word change; the actor's own API is already pub.
impl<T> ChargePointRuntime<T> {
    pub fn actor(&self) -> ChargePointActor { /* unchanged */ }
}
```

```rust
// Option B: builder methods that spawn/install these for the caller, matching how every other
// functional block is registered - i.e. the shape callers already understand.
impl<T, X> ChargePointBuilder<T, X> {
    /// Spawns `keepalive::run_ping_interval_updates` against this charge point's actor.
    pub async fn keepalive<P: PingIntervalControl + Send + Sync + 'static>(self, control: P) -> Self;

    /// Installs a watchdog, fed once per applied event exactly as `spawn_with_watchdog` does.
    pub fn watchdog<W: Watchdog + Send + Sync + 'static>(self, watchdog: W) -> Self;
}
```

Note that `watchdog` has to be settable *before* the actor spawns, so if the builder spawns eagerly
it may need to hold the value until `build()` — worth checking, and it is the reason we are not
proposing a post-`build()` setter.

**Done looks like:** a `ChargePointBuilder`-driven charge point can (a) have a CSMS change
`WebSocketPingInterval` and see the ping interval actually change, and (b) install a `Watchdog` whose
`pet` is called once per applied event. A test asserting a counter in a fake `Watchdog` advances as
events are applied would cover (b) exactly.

---

## Request 2 — a `HardwareCommand` variant that can carry power direction

**What is blocked:** every CSMS-driven V2G/bidirectional scenario. Not "is awkward" — cannot be
expressed.

### The situation

`HardwareCommand` (`src/state/event.rs:961`) has six variants: `LockConnector`, `UnlockConnector`,
`CloseContactor`, `OpenContactor`, `Reboot`, `SetCurrentLimit`. None can carry a direction.

`ChargePointBuilder::der_control` (`src/builder.rs:1908`) registers five handlers
(`SetDERControl`/`ClearDERControl`/`GetDERControl`/`AFRRSignal`/`NotifyAllowedEnergyTransfer`) and
updates state; nothing projects that state onto hardware, and — per the above — nothing could. Your
own module docs are explicit that the block "stores and reports … rather than actuating", so this is
a scope statement rather than an oversight, and the request is to widen the scope.

Our side is ready: our simulated meter genuinely discharges (signed `power_w`/`current_ma`, a
separate exported-energy register so OCPP's monotonic `Energy.Active.Import.Register` never runs
backwards), and a connector's direction is settable at runtime. What we cannot do is have a *CSMS*
cause any of it. Our frontend therefore exposes discharge as a deliberately local, out-of-band
action, and our docs have to say "CSMS tells the charger to export cannot be simulated at all".

### What would unblock it

`SetCurrentLimit` is the precedent to copy, end to end:

```rust
// src/state/event.rs
pub enum HardwareCommand {
    // …existing six…
    /// Set the direction energy flows in for this connector.
    SetPowerDirection {
        evse_id: usize,
        connector_id: usize,
        direction: PowerDirection, // Import | Export
    },
}
```

```rust
// src/hardware/connector.rs - alongside `set_current_limit`, whose Option<u32> signature and
// `Result<(), Self::Error>` contract this should mirror exactly.
async fn set_power_direction(&self, direction: PowerDirection) -> Result<(), Self::Error>;
```

Then the two pieces that make it real:

1. **The projection** in `src/hardware/command_executor.rs` (the match at ~line 60): map the new
   command to `connector.set_power_direction(..)` and confirm it with a new
   `ConnectorEvent::PowerDirectionConfirmed(direction)`, exactly as `SetCurrentLimit` maps to
   `set_current_limit` → `CurrentLimitConfirmed(limit_ma)`.
2. **The DER block emitting it**, so a `SetDERControl` that changes allowed energy transfer produces
   the command. This is the actual design question in this request, and it is yours: which DER
   control messages should project, and what a charge point that declares
   `supports_bidirectional_power: false` should do with one (we would expect a rejection rather than
   a silent no-op).

A default implementation returning `Ok(())` on the `Connector` trait would keep this from being a
breaking change for integrators who don't implement it — though note that would collide with this
crate's own "`Ok` means the hardware moved" stance, so an `Err` default, or a genuinely breaking
addition, may be the more honest choice. Your call; we implement either.

**Done looks like:** a CSMS sends a DER control that asks for export, and a `Connector`
implementation's `set_power_direction` is called with `Export`. Our simulator's meter then does the
rest, and we can delete the "blocked upstream" entry from our roadmap.

---

## Request 3 — sent/received message counters on `ChargePointState`

**What is blocked:** any traffic indicator in a frontend.

### The situation

`ChargePointState` (`src/state/charge_point_state.rs`) exposes `lifecycle`, `registration`, `evses`,
`next_transaction_id`, `local_authorization_list`, `pending_reset`, `device_model`, `capabilities`,
`network_profiles`, `authorization_cache`, `charging_profiles`, `tariffs` — and no message counts.

Our dashboard has no traffic indicator as a result. The quantities we *can* count on our side
(commands dispatched, state snapshots received) are not message counts, and labelling them as such
would misrepresent them, so we show nothing. For an OCPP tool whose main selling point is the
protocol trace, "is anything actually going over the wire right now" is the question a header most
wants to answer.

### What would unblock it

Anything monotonic and cheap. Two counters would do:

```rust
pub struct ChargePointState {
    // …existing…
    /// OCPP messages sent to the CSMS since this charge point started.
    pub messages_sent: u64,
    /// OCPP messages received from the CSMS since this charge point started.
    pub messages_received: u64,
}
```

Per-action counts (a small map keyed by action name) would be more useful still — "42 Heartbeats, 3
MeterValues" — but plain totals are enough to unblock us, and are the cheaper thing to maintain.

Two things worth deciding deliberately, because they change what the number means:

- Whether a *queued* message (offline queue) counts as sent when it is enqueued or when it goes out.
  We would render the second; the first would show traffic on a charge point with no connection.
- Whether these reset across a reconnect. We would prefer "since start", so a flapping connection is
  visible as a rate rather than hidden by repeated resets.

**Done looks like:** the counters advance as messages flow and are visible through
`ChargePointRuntime::state()`/`subscribe()`, so a frontend polling snapshots can render a rate.

---

## Notes for whoever picks this up

- **We are on 0.1.0 and can move quickly.** Breaking changes are cheaper for us than workarounds:
  our own docs say signature churn is cheapest before downstream consumers exist, and we would rather
  adapt to a right-shaped API than build around a wrong-shaped one.
- **Requests 1 and 3 are additive.** Request 2 probably is not, and we would rather it were correct
  than compatible.
- **We are not asking for behavior in the fakes.** Every simulated behavior (a meter that discharges,
  a firmware installer paced by injected time, a certificate store) is ours and already exists. What
  is missing is a way for the protocol layer to reach it, or to be reached.
- If any of these is deliberately out of scope, that answer is genuinely useful too: we document
  upstream limits explicitly rather than papering over them, and "won't do, because X" lets us write
  the honest version instead of leaving it as an open question.
