use crate::actions::{HardwareAction, PaletteEntry};
use crate::logs::{LogBuffer, LogEntry};
use crate::screen::Screen;
use crate::text_field::TextField;
use charge_point_simulator_core::charger::{
    ChargePointEvent, ChargePointState, ChargerConfig, ChargerEntry, ChargerHardware, ChargerState,
    Command, CommandParameter, ConnectionProfile, ConnectionStore, ConnectorHardwareSnapshot,
    FakeFileTransfer, FakeFirmwareInstaller, FakeFirmwareVerifier, FileCertificateStore,
    FileStorage, FirmwareInstallStage, InFlightTransfer, OcppVersion, RunningCharger,
    SecurityProfile, SimulationMode, TransferProfile, apply_hardware_snapshot, apply_ocpp_state,
    build_ocpp_event_for_connector, connect_charger, start_local_charger,
};
use color_eyre::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

/// How long to wait for a keyboard event before redrawing anyway, so
/// externally-sourced log lines (tracing output from the simulator, and
/// eventually `ocpp-charge-point`) show up promptly even with no input.
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How a status message in the command bar should read: [`theme::ok`] or [`theme::error`].
/// Carried alongside the message text itself rather than left for the renderer to infer from
/// the message's content (the previous approach parsed a `✗` prefix out of the string, which
/// broke silently for any message that didn't happen to start with it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSeverity {
    Ok,
    Error,
}

/// How long a status message stays in the command bar before expiring. Until Phase 5 these
/// persisted forever, which permanently hid the keybinding hint line that shares the bar -
/// the very line telling a new user how to do anything else.
const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(4);

/// A transient message in the command bar, carrying when it was shown so it can expire (see
/// [`STATUS_MESSAGE_TTL`]). Expiry is evaluated in `App::tick_metrics_with`, which is handed
/// `now` by its caller rather than reading the clock itself - the same injectable-clock
/// pattern the metrics tick already uses, so tests drive expiry with a chosen instant instead
/// of sleeping.
#[derive(Debug, Clone)]
pub struct Toast {
    pub severity: StatusSeverity,
    pub message: String,
    pub shown_at: Instant,
}

/// Identifies one connector within the currently loaded charger: which EVSE, and which
/// connector within it (both 0-indexed into [`ChargerState::evses`]/`EvseState::connectors`).
///
/// Replaces the old `focused_evse: usize` - commands (see [`App::available_commands`] and
/// [`App::apply_command`]) now dispatch against exactly the connector this points at, not just
/// "the first eligible connector on the focused EVSE." `Default` points at the first connector
/// of the first EVSE, matching a freshly selected charger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FocusedConnector {
    pub evse: usize,
    pub connector: usize,
}

#[derive(Debug, Default)]
pub struct App {
    pub screen: Screen,
    pub chargers: Vec<ChargerEntry>,
    /// Indexes into [`Self::filtered_chargers`], not `chargers` directly.
    pub selected_charger: usize,
    pub picker_filter: TextField,
    pub charger_state: Option<ChargerState>,
    pub focused: FocusedConnector,
    pub logs: LogBuffer,
    pub log_receiver: Option<UnboundedReceiver<LogEntry>>,
    /// Whether `/` has opened the log filter prompt. While open, typed characters narrow the
    /// log pane live rather than reaching the dashboard's other bindings.
    pub log_filter_open: bool,
    pub log_filter_field: TextField,
    pub command_palette_open: bool,
    pub command_palette_selected: usize,
    pub command_palette_filter: TextField,
    pub parameter_prompt: Option<Command>,
    pub parameter_field: TextField,
    /// The last value accepted for each parameter, prefilled the next time that same parameter
    /// is prompted for. Keyed by [`CommandParameter`] rather than by [`Command`] so commands
    /// sharing a parameter share its history.
    pub parameter_history: HashMap<CommandParameter, String>,
    /// Why the current parameter value was rejected, shown inline under the prompt. Cleared as
    /// soon as the value changes, so a stale complaint never outlives what it was about.
    pub parameter_error: Option<&'static str>,
    pub help_open: bool,
    /// How many lines the help overlay is scrolled down by, for terminals too short to show
    /// the whole keybinding table at once.
    pub help_scroll: usize,
    pub quit_confirm_open: bool,
    pub status_message: Option<Toast>,
    pub connection_store: ConnectionStore,
    pub connection_store_path: Option<PathBuf>,
    pub connection_csms_url: TextField,
    pub connection_ocpp_identity: TextField,
    pub connection_password: TextField,
    pub connection_focused_field: usize,
    /// Why the typed CSMS URL was rejected, shown inline under the field - cleared as soon as
    /// the URL field is edited, the same rule [`Self::parameter_error`] follows.
    pub connection_url_error: Option<&'static str>,
    /// Whether the password field shows its raw value instead of `*`s. Starts (and resets to)
    /// `false` every time the connection setup screen is entered - a revealed password
    /// shouldn't survive to the *next* charger's setup screen just because this one was
    /// toggled.
    pub connection_password_revealed: bool,
    /// Which entry of [`Self::connection_url_suggestions`] `PageUp`/`PageDown` last landed the
    /// URL field on, so cycling continues from there instead of always restarting at the first
    /// suggestion. Reset whenever the URL field is edited, since the previous index may no
    /// longer point at the same suggestion once the (filtered) list changes.
    pub connection_url_suggestion: Option<usize>,
    pub connect_result_receiver: Option<oneshot::Receiver<Result<(), String>>>,
    /// Live snapshots forwarded from a running charger's background thread - dialed against a real
    /// CSMS, or (H3b) running entirely locally with none at all - drained each frame by
    /// [`Self::drain_charger_snapshots`].
    pub charger_snapshot_receiver: Option<UnboundedReceiver<ChargerSnapshot>>,
    /// Where dispatched commands go once a charger's background thread is running, whether that
    /// charger has a live CSMS on the other end or not (see [`Self::apply_command`]).
    pub ocpp_event_sender: Option<UnboundedSender<ChargePointEvent>>,
    /// Forwards this frame's simulated `elapsed` to the running charger's background thread, so
    /// it can call `RunningCharger::tick` - the single entry point through which a charger's
    /// meter moves, for a local charger and a live-CSMS one alike (H3b). `None` until a charger's
    /// background thread has been spawned - see [`Self::spawn_local_charger`]/
    /// [`Self::confirm_connection_setup`].
    pub ocpp_tick_sender: Option<UnboundedSender<Duration>>,
    /// Why the last CSMS connection attempt failed, kept until something is actually done about it -
    /// rendered as the dashboard's `Connection` strip, and the reason the header stops trusting
    /// `ChargerState::connection_status` (see [`crate::ui::dashboard::LinkState`]).
    ///
    /// Deliberately *not* a [`Toast`]: those expire after [`STATUS_MESSAGE_TTL`], which is right for
    /// "command dispatched" and wrong for this. A failed dial leaves a charger that will never
    /// connect, never report anything, and never change on its own - the one state on this screen
    /// that has to outlive a four-second timer, because nothing else will ever mention it again.
    ///
    /// Cleared by the three things that genuinely address it: a retry
    /// ([`Self::retry_connection`]), a successful connection, and leaving for the picker.
    pub connection_failure: Option<String>,
    /// The charger-wide firmware/file-transfer activity from the most recent snapshot, rendered as
    /// the dashboard's activity strip. `CampaignProgress::default()` (everything `None`) both before
    /// any snapshot arrives and for a charger with no such hardware at all.
    pub campaigns: CampaignProgress,
    /// Sends direct hardware actions to the running charger's background thread - today only V2G
    /// discharge, which no OCPP message can express. `None` until a charger's thread has been
    /// spawned, the same as [`Self::ocpp_tick_sender`]. See [`HardwareControl`].
    pub hardware_control_sender: Option<UnboundedSender<HardwareControl>>,
    /// The most recent protocol state out of `charger_snapshot_receiver`, used to decide what event
    /// a dispatched command maps to (see
    /// [`charge_point_simulator_core::charger::build_ocpp_event_for_connector`]).
    pub live_ocpp_state: Option<ChargePointState>,
    /// When [`Self::tick_metrics`] last ran, so it can compute real elapsed time between
    /// frames rather than assuming a fixed interval (the main loop's actual cadence varies
    /// with input activity).
    pub last_metrics_tick: Option<Instant>,
    /// The terminal area the last frame was drawn into, stashed by [`Self::draw`] so
    /// [`Self::handle_mouse_event`] can hit-test clicks against the exact layout that frame
    /// used, without an extra `crossterm::terminal::size()` call (and the "no real terminal in
    /// tests" problem that would bring).
    pub last_frame_area: Rect,
    pub exit: bool,
}

/// Decides what `ChargerState::mode` should become from the (possibly blank, possibly
/// whitespace-padded) CSMS URL field on the connection setup screen. A blank URL means "no
/// CSMS, run locally"; anything else means a real connection attempt is about to be made.
///
/// Kept as a free function, separate from `App::confirm_connection_setup`, specifically so it
/// can be unit tested as a plain, synchronous decision - without going anywhere near the
/// background thread `confirm_connection_setup` spawns to actually perform the connection.
/// Validates a parameter value, returning the reason it's unacceptable or `None` if it's fine.
///
/// Kept a free function, like [`resolve_simulation_mode`], so the rule can be tested as a plain
/// decision without standing up an `App` and a prompt around it. Every parameter the app has
/// today (vehicle id, RFID tag, fault code, display message) is free text whose only real
/// requirement is being present, so this is deliberately one rule rather than a per-parameter
/// table - add that when a parameter actually needs a different rule.
fn validate_parameter(value: &str) -> Option<&'static str> {
    if value.trim().is_empty() {
        return Some("cannot be blank");
    }
    None
}

/// Validates the CSMS URL field on the connection setup screen. Blank is fine - that's "run
/// locally," see [`resolve_simulation_mode`] - but a non-blank value must be a WebSocket URL,
/// since that's the only scheme OCPP ever dials a CSMS over; anything else is almost certainly
/// a typo (a pasted `https://` dashboard link, most often) worth catching before a connection
/// attempt fails on it.
fn validate_csms_url(value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        return None;
    }
    Some("must start with ws:// or wss://")
}

/// Whether `(col, row)` falls within `area` - used to gate log pane wheel-scrolling on the
/// mouse actually being over the log pane, rather than scrolling it from anywhere on screen.
fn rect_contains(area: Rect, col: u16, row: u16) -> bool {
    area.contains(ratatui::layout::Position::new(col, row))
}

fn resolve_simulation_mode(csms_url: &str) -> SimulationMode {
    if csms_url.trim().is_empty() {
        SimulationMode::Local
    } else {
        SimulationMode::LiveCsms {
            url: csms_url.to_string(),
        }
    }
}

/// Everything about a running charger the dashboard can observe, as one owned value that crosses
/// the thread boundary between the charger and the UI.
///
/// Two halves, because they come from two different places and only one of them exists in the OCPP
/// protocol at all:
///
/// - `ocpp` is a `ChargePointState` snapshot, applied with `apply_ocpp_state`.
/// - `hardware` is [`RunningCharger::hardware_snapshot`], applied with `apply_hardware_snapshot` -
///   per-connector lock, contactor, applied current limit (`docs/hardware-roadmap.md`'s H7), power
///   direction and exported-energy register (H14). None of these appear in a `ChargePointState`.
///
/// A caller on the charger's own thread would just call `RunningCharger::apply_state` and get both
/// at once. The TUI can't: `connect_charger`'s future isn't `Send`, so the charger runs on its own
/// thread (see [`App::spawn_local_charger`]) and the hardware handle never leaves it. Forwarding
/// this is what stands in for that call - see [`RunningCharger::hardware_snapshot`]'s own doc
/// comment, and `core`'s `a_hardware_snapshot_carries_what_apply_state_would_have_written` for the
/// proof that the two paths agree.
#[derive(Debug, Clone)]
pub struct ChargerSnapshot {
    pub ocpp: ChargePointState,
    pub hardware: Vec<Vec<ConnectorHardwareSnapshot>>,
    /// Charger-wide firmware/file-transfer activity, read off the bundle handles the charger's own
    /// thread keeps - see [`CampaignProgress`] and [`CampaignHandles`].
    pub campaigns: CampaignProgress,
}

impl ChargerSnapshot {
    fn of(running: &RunningCharger, campaigns: &CampaignHandles) -> Self {
        Self {
            ocpp: running.state(),
            hardware: running.hardware_snapshot(),
            campaigns: campaigns.progress(),
        }
    }
}

/// The charger-wide hardware activity the dashboard shows: a firmware campaign
/// (`docs/hardware-roadmap.md`'s H10) and the file transfers behind a firmware download or a
/// diagnostics log upload.
///
/// Charger-wide rather than per-connector because that is what the hardware is: one installer, one
/// file transfer per charger. Every field is `None` when the charger has no such hardware at all -
/// which is most chargers, since [`charger_hardware`] only builds a piece the charger's own
/// `capabilities:` block declares. `Some(FirmwareInstallStage::Idle)` is therefore meaningfully
/// different from `None`: the first is an installer with nothing to do, the second is a charger that
/// cannot install firmware.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CampaignProgress {
    pub firmware_install: Option<FirmwareInstallStage>,
    pub firmware_download: Option<InFlightTransfer>,
    pub log_upload: Option<InFlightTransfer>,
}

impl CampaignProgress {
    /// Whether anything is actually happening - a campaign in flight, or one that has finished with
    /// an outcome still worth showing. An installer sitting `Idle` is not activity.
    pub fn is_active(&self) -> bool {
        self.firmware_download.is_some()
            || self.log_upload.is_some()
            || matches!(
                self.firmware_install,
                Some(
                    FirmwareInstallStage::Installing
                        | FirmwareInstallStage::Installed
                        | FirmwareInstallStage::Failed
                )
            )
    }
}

/// The `Arc`s the charger's background thread keeps on the two pieces of its hardware bundle whose
/// progress is worth rendering.
///
/// Cloned out of the bundle *before* it is handed to `connect_charger`/`start_local_charger`, which
/// consume it whole - the "clone before handing ownership away" pattern `ChargerHardware`'s own doc
/// comment prescribes, and the only way to still have a handle afterwards. `RunningCharger` keeps
/// its own clones to tick, but exposes neither, so this is not redundant with anything reachable
/// through it.
#[derive(Default)]
struct CampaignHandles {
    firmware_installer: Option<Arc<FakeFirmwareInstaller>>,
    file_transfer: Option<Arc<FakeFileTransfer>>,
}

impl CampaignHandles {
    fn of(hardware: &ChargerHardware) -> Self {
        Self {
            firmware_installer: hardware.firmware_installer.clone(),
            file_transfer: hardware.file_transfer.clone(),
        }
    }

    fn progress(&self) -> CampaignProgress {
        CampaignProgress {
            firmware_install: self
                .firmware_installer
                .as_ref()
                .map(|installer| installer.stage()),
            firmware_download: self
                .file_transfer
                .as_ref()
                .and_then(|transfer| transfer.download_in_flight()),
            log_upload: self
                .file_transfer
                .as_ref()
                .and_then(|transfer| transfer.upload_in_flight()),
        }
    }
}

/// How long a simulated firmware installation takes. A deliberate product choice, not a physical
/// one: long enough that `Installing` is a state a CSMS developer can actually watch (and that a
/// `FirmwareStatusNotification` sequence has time to be observed in order), short enough that a demo
/// isn't spent waiting. Simulated time, forwarded by [`App::tick_metrics`] at the frame cadence, so
/// in the running app it works out to roughly this much wall clock.
const SIMULATED_FIRMWARE_INSTALL: Duration = Duration::from_secs(30);

/// The firmware image a simulated download claims to fetch. Sized and paced to read like a real
/// charger firmware image over a slow link, for the same reason as above - nothing here transfers
/// real bytes (see `FakeFileTransfer`'s module docs).
const SIMULATED_FIRMWARE_DOWNLOAD: TransferProfile = TransferProfile {
    duration: Duration::from_secs(20),
    total_bytes: 8 * 1024 * 1024,
};

/// The diagnostics bundle a simulated log upload claims to send - smaller and quicker than a
/// firmware image, as a real log archive is. Independently configured because a CSMS developer needs
/// to watch an upload and a download at once, which is exactly what `FakeFileTransfer` supports.
/// The stand-in log archive a locally-driven diagnostics upload sends. `FakeFileTransfer` never
/// moves real bytes and reports `SIMULATED_LOG_UPLOAD.total_bytes` as the size regardless, so this
/// exists to be *something* honest for `FakeFileTransfer::last_upload` to hold rather than to be a
/// realistic archive - a fabricated one would invite reading it as real diagnostics.
fn simulated_diagnostics_log() -> Vec<u8> {
    b"flowion-charge-point-simulator: simulated diagnostics archive\n".to_vec()
}

const SIMULATED_LOG_UPLOAD: TransferProfile = TransferProfile {
    duration: Duration::from_secs(10),
    total_bytes: 2 * 1024 * 1024,
};

/// The hardware bundle `config`'s charger runs with: storage and a display as before
/// (`ChargerHardware::new`, H5b/H6b), plus a firmware installer/verifier, a file transfer and a
/// certificate store for a charger whose `capabilities:` block declares the matching functional
/// block.
///
/// Gated on the declaration rather than handed over unconditionally, because each of these is a
/// scenario-specific choice `ChargerHardware::new` deliberately refuses to make a default for (see
/// its doc comment) - an install duration, a transfer size, a certificate limit. Passing hardware a
/// charger didn't declare registers nothing anyway (`register_optional_hardware` reads the same
/// capabilities), so the gate here is about not inventing a configuration nobody asked for, and
/// about the state directory staying empty for a charger that declares no persistence.
///
/// Which flag reaches which piece, matching `register_optional_hardware`'s own gating:
///
/// - `firmware_management` → the installer *and* the verifier. Without a verifier upstream refuses
///   signed updates outright (`NoFirmwareVerifier` fails closed), so a charger that can install
///   firmware but can't check a signature would only ever be able to demonstrate half the flow.
/// - `firmware_management` or `diagnostics` → the file transfer, which backs both a firmware
///   download and a log upload; one instance serves both, and both can be in flight at once.
/// - `certificate_management` → the certificate store, under its own subdirectory of the charger's
///   state directory so certificate keys can never collide with the runtime's own persisted keys.
///
/// `key_store` is deliberately left `None`: nothing registers a `KeyStore` (see `ChargerHardware`'s
/// doc comment - no `ChargePointBuilder` method takes one), so the only value a store here could
/// have is to a caller keeping its own `Arc` clone for TLS or certificate-renewal wiring of its own.
/// The TUI has none, and populating the field would suggest the charger does something with it.
fn charger_hardware(config: &ChargerConfig) -> ChargerHardware {
    let state_dir = charger_storage_dir(&config.id);
    let capabilities = &config.capabilities;

    let mut hardware = ChargerHardware::new(&state_dir);

    if capabilities.firmware_management {
        hardware.firmware_installer = Some(Arc::new(FakeFirmwareInstaller::new(
            SIMULATED_FIRMWARE_INSTALL,
        )));
        hardware.firmware_verifier = Some(Arc::new(FakeFirmwareVerifier::new()));
    }
    if capabilities.firmware_management || capabilities.diagnostics {
        hardware.file_transfer = Some(Arc::new(FakeFileTransfer::new(
            SIMULATED_FIRMWARE_DOWNLOAD,
            SIMULATED_LOG_UPLOAD,
        )));
    }
    if capabilities.certificate_management {
        hardware.certificate_store = Some(FileCertificateStore::new(FileStorage::new(
            state_dir.join("certificates"),
        )));
    }

    hardware
}

/// A direct action on a running charger's fake hardware, dispatched to its background thread
/// alongside (but separately from) the `ChargePointEvent`s [`App::apply_command`] sends.
///
/// Separate because these do not go through the protocol at all, and can't: `HardwareCommand` has
/// six variants and none of them can carry a power direction, so no CSMS message reaching this
/// charger could ever produce one (see `RunningCharger::set_discharging`'s own doc comment, and
/// `docs/hardware-roadmap.md`'s "Known gaps"). Keeping them in their own channel, with their own
/// type, is what stops a reader from assuming the dashboard's V2G control is something OCPP did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareControl {
    /// Put one connector into export (V2G discharge), or back to import. Addressed positionally -
    /// indices into `ChargerState::evses`/`EvseState::connectors`, exactly as
    /// `RunningCharger::set_discharging` addresses it.
    SetDischarging {
        evse: usize,
        connector: usize,
        discharging: bool,
    },
    /// Fetch a firmware image and install it, with no CSMS campaign behind either half - see
    /// [`SIMULATED_FIRMWARE_URL`] and `FakeFirmwareInstaller::run_install`. The download is run
    /// first because that is the order a real campaign runs it in, and because it is what makes the
    /// campaign strip show a download before an install.
    InstallFirmware,
    /// Render a diagnostics log archive and upload it, likewise with no CSMS behind it.
    UploadDiagnostics,
    /// Arm the installer to fail. Not undoable - see [`crate::actions::HardwareAction::description`].
    FailFirmwareInstall,
    /// Arm the download half of the file transfer to fail.
    FailFirmwareDownload,
    /// Arm the upload half to fail, independently of the download.
    FailDiagnosticsUpload,
}

/// Where a locally-driven firmware image is fetched from, and where a locally-driven log archive is
/// sent. Both are `.invalid` (RFC 2606's reserved TLD, guaranteed never to resolve) and never
/// dialed: `FakeFileTransfer` moves no real bytes, and a URL that looked real would invite someone
/// to check whether it was.
const SIMULATED_FIRMWARE_URL: &str = "https://firmware.invalid/flowion-simulator.bin";
const SIMULATED_DIAGNOSTICS_URL: &str = "https://diagnostics.invalid/upload";

/// The line a dispatched [`HardwareControl`] gets in the log pane and the status bar. Written from
/// the control rather than the action so it can name the connector a discharge toggle resolved to,
/// which is the part a reader needs to check.
fn hardware_control_log_line(control: HardwareControl, charger: &ChargerState) -> String {
    match control {
        HardwareControl::SetDischarging {
            evse,
            connector,
            discharging,
        } => {
            let evse_id = charger.evses.get(evse).map(|evse| evse.id);
            let connector_id = charger
                .evses
                .get(evse)
                .and_then(|evse| evse.connectors.get(connector))
                .map(|connector| connector.id);
            let target = match (evse_id, connector_id) {
                (Some(evse_id), Some(connector_id)) => {
                    format!("EVSE {evse_id} connector {connector_id}")
                }
                _ => "connector".to_string(),
            };
            let direction = if discharging {
                "exporting (V2G)"
            } else {
                "importing"
            };
            format!("{target}: {direction}")
        }
        HardwareControl::InstallFirmware => "firmware update started (no CSMS)".to_string(),
        HardwareControl::UploadDiagnostics => "diagnostics upload started (no CSMS)".to_string(),
        HardwareControl::FailFirmwareInstall => "firmware installs will now fail".to_string(),
        HardwareControl::FailFirmwareDownload => "firmware downloads will now fail".to_string(),
        HardwareControl::FailDiagnosticsUpload => "diagnostics uploads will now fail".to_string(),
    }
}
/// Runs a [`RunningCharger`] to completion: publishes a [`ChargerSnapshot`] to `snapshot_sender`
/// whenever anything observable moves, applies every dispatched command from `event_receiver`, and
/// calls `RunningCharger::tick` for every simulated `elapsed` forwarded on `tick_receiver` - the
/// one driving loop a charger's background thread runs, identical whether `running` came from
/// [`connect_charger`] (a real CSMS on the other end) or [`start_local_charger`] (nothing at all).
/// That sameness is the point of `docs/hardware-roadmap.md`'s H3b: a local and a connected charger
/// differ only in how `running` was built, never in how it's driven afterwards.
///
/// Returns once every one of `snapshot_sender`/`event_receiver`/`tick_receiver`'s `App`-side
/// counterpart has been dropped (see [`App::return_to_picker`]), which is what lets the spawning
/// thread exit instead of running forever against a charger that's no longer shown.
async fn drive_running_charger(
    running: RunningCharger,
    snapshot_sender: UnboundedSender<ChargerSnapshot>,
    mut event_receiver: UnboundedReceiver<ChargePointEvent>,
    mut tick_receiver: UnboundedReceiver<Duration>,
    mut control_receiver: UnboundedReceiver<HardwareControl>,
    campaigns: CampaignHandles,
) {
    // Published proactively rather than waiting for the first `changed()`: `subscribe()` only
    // yields *future* changes, and a charger nothing has happened to yet (freshly started, no
    // commands, no ticks) might never produce one on its own - which would leave
    // `apply_ocpp_state` never having run at all. A `Local` charger's `Offline` status (H3b)
    // depends on it having run at least once, even for a charger sitting idle.
    let _ = snapshot_sender.send(ChargerSnapshot::of(&running, &campaigns));

    let mut states = running.subscribe();
    let state_snapshots = snapshot_sender.clone();
    let forward_states = async {
        loop {
            states.changed().await;
            if state_snapshots
                .send(ChargerSnapshot::of(&running, &campaigns))
                .is_err()
            {
                break;
            }
        }
    };
    let forward_commands = async {
        while let Some(event) = event_receiver.recv().await {
            let _ = running.send(event).await;
        }
    };
    // Ticking publishes a snapshot too, not only `states.changed()` above: the hardware-only half
    // of a snapshot (H7's lock/contactor/current limit, H14's direction and exported-energy
    // register) has no `ChargePointState` counterpart, so nothing about it is guaranteed to bump
    // the state version. The export register in particular rises on every single tick while
    // discharging without the protocol state moving at all, and would otherwise only reach the
    // screen the next time something unrelated happened to change.
    let tick_snapshots = snapshot_sender.clone();
    let forward_ticks = async {
        while let Some(elapsed) = tick_receiver.recv().await {
            running.tick(elapsed).await;
            if tick_snapshots
                .send(ChargerSnapshot::of(&running, &campaigns))
                .is_err()
            {
                break;
            }
        }
    };
    // Hardware actions publish a snapshot of their own rather than waiting for the next tick, so
    // the dashboard reflects a V2G toggle on the very frame it was pressed. `set_discharging`
    // returns `Err` for a connector this charger doesn't have; the UI only ever addresses the
    // connector it is focused on, so that would be a bug here rather than user error - logged, not
    // surfaced as a status message.
    let forward_controls = async {
        while let Some(control) = control_receiver.recv().await {
            match control {
                HardwareControl::SetDischarging {
                    evse,
                    connector,
                    discharging,
                } => {
                    if let Err(error) = running.set_discharging(evse, connector, discharging) {
                        tracing::warn!(%error, "hardware control addressed a connector that does not exist");
                        continue;
                    }
                }
                // Spawned rather than awaited: a campaign takes tens of seconds of simulated time,
                // and awaiting it here would leave every later control (a V2G toggle, an armed
                // failure) queued behind it. The spawned task holds only `Arc` clones of the
                // hardware, never `running` - which is not `Send` and could not cross a task
                // boundary anyway. Progress reaches the screen through the per-tick snapshot above.
                HardwareControl::InstallFirmware => {
                    let (Some(transfer), Some(installer)) = (
                        campaigns.file_transfer.clone(),
                        campaigns.firmware_installer.clone(),
                    ) else {
                        tracing::warn!(
                            "a firmware update was requested on a charger with no firmware hardware"
                        );
                        continue;
                    };
                    tokio::spawn(async move {
                        // Download then install, the order a real campaign runs them in. A failed
                        // download stops there: installing an image that never arrived would be a
                        // fiction, and the armed failure exists precisely to test that path.
                        if let Err(error) = transfer.run_download(SIMULATED_FIRMWARE_URL).await {
                            tracing::warn!(%error, "simulated firmware download failed");
                            return;
                        }
                        if let Err(error) = installer.run_install().await {
                            tracing::warn!(%error, "simulated firmware installation failed");
                        }
                    });
                }
                HardwareControl::UploadDiagnostics => {
                    let Some(transfer) = campaigns.file_transfer.clone() else {
                        tracing::warn!(
                            "a diagnostics upload was requested on a charger with no file transfer"
                        );
                        continue;
                    };
                    tokio::spawn(async move {
                        if let Err(error) = transfer
                            .run_upload(SIMULATED_DIAGNOSTICS_URL, simulated_diagnostics_log())
                            .await
                        {
                            tracing::warn!(%error, "simulated diagnostics upload failed");
                        }
                    });
                }
                HardwareControl::FailFirmwareInstall => {
                    if let Some(installer) = &campaigns.firmware_installer {
                        installer.trigger_failure();
                    }
                }
                HardwareControl::FailFirmwareDownload => {
                    if let Some(transfer) = &campaigns.file_transfer {
                        transfer.trigger_download_failure();
                    }
                }
                HardwareControl::FailDiagnosticsUpload => {
                    if let Some(transfer) = &campaigns.file_transfer {
                        transfer.trigger_upload_failure();
                    }
                }
            }
            if snapshot_sender
                .send(ChargerSnapshot::of(&running, &campaigns))
                .is_err()
            {
                break;
            }
        }
    };
    tokio::join!(
        forward_states,
        forward_commands,
        forward_ticks,
        forward_controls
    );
}

/// Where `charger_id`'s persisted hardware state (in-flight transaction, boot reason, cached
/// device model, ...) lives on disk - `ChargerHardware::new`'s `FileStorage` is rooted here.
///
/// `FileStorage` deliberately "stays decoupled from `dirs`" (see its own module docs) and takes
/// an explicit directory instead, so resolving one is this call site's job - the same split
/// `main.rs`'s `connection_store_path` already draws for `ConnectionStore`: one subdirectory per
/// charger under the same `flowion-charge-point-simulator` config root `connections.yaml` lives
/// in, honoring the same `FLOWION_STATE_DIR` override so tests/CI can redirect both without
/// touching the real home directory.
fn charger_storage_dir(charger_id: &str) -> PathBuf {
    if let Ok(path) = std::env::var("FLOWION_STATE_DIR") {
        return PathBuf::from(path).join("storage").join(charger_id);
    }
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("flowion-charge-point-simulator")
        .join("storage")
        .join(charger_id)
}

impl App {
    pub fn new(chargers: Vec<ChargerEntry>) -> Self {
        Self {
            chargers,
            ..Default::default()
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.drain_log_receiver();
            self.drain_charger_snapshots();
            self.poll_connect_result();
            self.tick_metrics();
            terminal.draw(|frame| self.draw(frame))?;
            if self.exit {
                break;
            }
            if event::poll(INPUT_POLL_INTERVAL)? {
                self.handle_events()?;
            }
        }
        Ok(())
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame) {
        // Stashed so `handle_mouse_event` can hit-test against the same layout this frame was
        // actually drawn with, instead of re-querying the terminal's current size - which would
        // both add an extra syscall per click and, in tests, have no real terminal to query at
        // all (see `handle_mouse_event`'s tests, which set this field directly).
        self.last_frame_area = frame.area();
        crate::ui::draw(frame, self);
    }

    /// Commands eligible to run against the currently focused connector (see
    /// [`FocusedConnector`]) - not merely the focused EVSE's first eligible connector, so the
    /// palette never offers something that would silently act on a different connector than
    /// the one on screen.
    pub(crate) fn available_commands(&self) -> Vec<Command> {
        let Some(state) = &self.charger_state else {
            return Vec::new();
        };
        let connector = state
            .evses
            .get(self.focused.evse)
            .and_then(|evse| evse.connectors.get(self.focused.connector));
        Command::ALL
            .into_iter()
            .filter(|command| {
                if command.is_display_command() {
                    command.is_available_for_charger(state)
                } else {
                    connector.is_some_and(|connector| command.is_available_for_connector(connector))
                }
            })
            .collect()
    }

    /// [`available_commands`](Self::available_commands) further narrowed by the command
    /// palette's filter text, matched as a fuzzy subsequence (see [`crate::fuzzy::score`])
    /// rather than a plain substring: `"pv"` finds "Plug in vehicle".
    ///
    /// Results are ordered best-match first, with ties broken by the order in `Command::ALL`
    /// so a given filter always produces the same list - `sort_by_key` is stable, which is
    /// what makes that guarantee hold.
    pub(crate) fn palette_commands(&self) -> Vec<PaletteEntry> {
        let filter = self.command_palette_filter.value();
        let mut scored: Vec<(PaletteEntry, u32)> = self
            .palette_entries()
            .into_iter()
            .filter_map(|entry| {
                crate::fuzzy::score(entry.label(), filter).map(|score| (entry, score))
            })
            .collect();
        scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(entry, _)| entry).collect()
    }

    /// Everything the palette could offer right now: the eligible protocol
    /// [`commands`](Self::available_commands), then the eligible [`HardwareAction`]s (see
    /// [`crate::actions`] for why those are a separate kind of thing). Both are filtered by their
    /// own availability rules, so an action a charger hasn't declared is never listed - not listed
    /// and greyed out, simply absent, the same treatment an ineligible command already gets.
    pub(crate) fn palette_entries(&self) -> Vec<PaletteEntry> {
        let Some(state) = &self.charger_state else {
            return Vec::new();
        };
        let connector = state
            .evses
            .get(self.focused.evse)
            .and_then(|evse| evse.connectors.get(self.focused.connector));

        self.available_commands()
            .into_iter()
            .map(PaletteEntry::Command)
            .chain(
                HardwareAction::ALL
                    .into_iter()
                    .filter(|action| action.is_available(state, connector, &self.campaigns))
                    .map(PaletteEntry::Hardware),
            )
            .collect()
    }

    fn handle_events(&mut self) -> Result<()> {
        match event::read()? {
            // it's important to check that the event is a key press event as
            // crossterm also emits key release and repeat events on Windows.
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                self.handle_key_event(key_event)
            }
            // Only the mouse events this app actually acts on are forwarded - in particular
            // not `MouseEventKind::Moved`/`Drag`, which fire continuously while mouse capture
            // is on (see `main.rs`) and would otherwise force a redraw on every pixel of mouse
            // movement for no visible effect.
            Event::Mouse(mouse_event)
                if matches!(
                    mouse_event.kind,
                    MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) =>
            {
                self.handle_mouse_event(mouse_event)
            }
            _ => {}
        };
        Ok(())
    }

    /// `pub(crate)` so snapshot scenarios can drive the app through real key presses rather
    /// than hand-setting the state those presses produce.
    pub(crate) fn handle_key_event(&mut self, key_event: KeyEvent) {
        if self.quit_confirm_open {
            self.handle_quit_confirm_key(key_event);
            return;
        }
        if self.parameter_prompt.is_some() {
            self.handle_parameter_prompt_key(key_event);
            return;
        }
        if self.command_palette_open {
            self.handle_command_palette_key(key_event);
            return;
        }
        if self.help_open {
            self.handle_help_key(key_event);
            return;
        }
        // Before the global 'q'/'?' shortcuts: both are typeable into a log filter.
        if self.log_filter_open {
            self.handle_log_filter_key(key_event);
            return;
        }

        // On the connection setup screen 'q'/'?' need to be typeable (URLs and passwords can
        // contain either), so the global shortcuts don't apply there. On the picker, 'q' also
        // needs to be typeable into the charger filter (Esc still quits when the filter's
        // empty - see `handle_pick_charger_key`); '?' stays global there since charger ids
        // never contain it and losing Help on the very first screen would be worse.
        if self.screen != Screen::ConnectionSetup {
            match key_event.code {
                KeyCode::Char('q') if self.screen != Screen::PickCharger => {
                    self.quit_confirm_open = true;
                    return;
                }
                KeyCode::Char('?') => {
                    self.help_open = true;
                    return;
                }
                _ => {}
            }
        }

        match self.screen {
            Screen::PickCharger => self.handle_pick_charger_key(key_event),
            Screen::ConnectionSetup => self.handle_connection_setup_key(key_event),
            Screen::Dashboard => self.handle_dashboard_key(key_event),
        }
    }

    /// `pub(crate)` for the same reason as [`Self::handle_key_event`]: tests drive it directly.
    ///
    /// Follows the same modal-then-screen gating `handle_key_event` uses, but narrower: the
    /// modals mouse support was actually asked for (Phase 7: tree focus, log wheel-scroll,
    /// palette row clicks) are the command palette and the dashboard, so every other modal
    /// (quit confirm, the parameter prompt, help, the log filter prompt) simply ignores mouse
    /// input for now rather than guessing what a click there should do.
    pub(crate) fn handle_mouse_event(&mut self, mouse_event: MouseEvent) {
        if self.quit_confirm_open
            || self.parameter_prompt.is_some()
            || self.help_open
            || self.log_filter_open
        {
            return;
        }
        if self.command_palette_open {
            self.handle_command_palette_mouse(mouse_event);
            return;
        }
        if self.screen == Screen::Dashboard {
            self.handle_dashboard_mouse(mouse_event);
        }
    }

    /// Click-to-focus on the EVSE/connector tree, and wheel-scroll on the log pane - both
    /// hit-tested against the same layout `crate::ui::dashboard::render` computes from the
    /// current terminal size, since neither `App` nor the render path stashes the rects a frame
    /// last used (see the roadmap's "the view model is thin" note for why: everything here is
    /// recomputed fresh, the same way a redraw is).
    fn handle_dashboard_mouse(&mut self, mouse_event: MouseEvent) {
        use crate::ui::dashboard;

        let area = self.last_frame_area;
        if dashboard::is_terminal_too_small(area) {
            return;
        }
        let layout = dashboard::dashboard_layout(area);

        match mouse_event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // The same strips `dashboard::render` lays the body out from, so a click is
                // hit-tested against exactly the rows the last frame drew - each strip shifts the
                // tree down while it's on screen.
                let body = dashboard::body_layout(
                    layout.body,
                    dashboard::body_strips_for(
                        self.charger_state.as_ref(),
                        &self.campaigns,
                        self.connection_failure.as_deref(),
                    ),
                );
                self.handle_tree_click(body.tree, body.sidebar.is_none(), mouse_event);
            }
            MouseEventKind::ScrollUp
                if rect_contains(layout.log, mouse_event.column, mouse_event.row) =>
            {
                self.logs.scroll_up();
            }
            MouseEventKind::ScrollDown
                if rect_contains(layout.log, mouse_event.column, mouse_event.row) =>
            {
                self.logs.scroll_down();
            }
            _ => {}
        }
    }

    /// Focuses the connector, if any, that `mouse_event` landed on within the tree's `tree_area`,
    /// via the reverse of `dashboard::tree_focus_line_index`:
    /// [`dashboard::tree_line_to_connector`](crate::ui::dashboard::tree_line_to_connector).
    /// Clicking an EVSE summary row, a "no connectors"/inline-detail filler line, or outside the
    /// tree entirely does nothing - there's no connector there to focus.
    fn handle_tree_click(&mut self, tree_area: Rect, inline_detail: bool, mouse_event: MouseEvent) {
        use crate::ui::dashboard;

        let Some(state) = &self.charger_state else {
            return;
        };
        let Some(content_row) =
            dashboard::content_row_at(tree_area, mouse_event.column, mouse_event.row)
        else {
            return;
        };
        let tree_height = tree_area.height.saturating_sub(1) as usize;
        let offset =
            dashboard::tree_scroll_offset_for(state, self.focused, inline_detail, tree_height);
        let line_index = content_row + offset;

        if let Some((evse, connector)) =
            dashboard::tree_line_to_connector(state, self.focused, inline_detail, line_index)
        {
            self.focused = FocusedConnector { evse, connector };
        }
    }

    /// Clicking a command palette row selects it, the same as arrow-keying to it - it does
    /// *not* dispatch immediately. Consistent with `↑`/`↓` (which only move the selection;
    /// `Enter` is what actually runs a command) and deliberately safer than click-to-activate:
    /// several palette commands mutate simulated state (plug in a vehicle, report a fault), and
    /// a misclick shouldn't be able to fire one with no chance to see the target line first.
    fn handle_command_palette_mouse(&mut self, mouse_event: MouseEvent) {
        if !matches!(mouse_event.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }
        let area = self.last_frame_area;
        let commands = self.palette_commands();
        let popup = crate::ui::palette::palette_popup_rect(area, commands.len());
        let list_area = crate::ui::palette::palette_list_area(popup);

        if let Some(index) = crate::ui::palette::command_index_at(
            list_area,
            mouse_event.column,
            mouse_event.row,
            commands.len(),
        ) {
            self.command_palette_selected = index;
        }
    }

    fn handle_quit_confirm_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Char('y') | KeyCode::Enter => self.exit = true,
            KeyCode::Char('n') | KeyCode::Esc => self.quit_confirm_open = false,
            _ => {}
        }
    }

    /// Opens the log filter prompt, seeded with whatever filter is currently applied so
    /// refining one doesn't mean retyping it.
    fn open_log_filter(&mut self) {
        self.log_filter_field = TextField::default();
        for c in self.logs.filter().unwrap_or_default().chars() {
            self.log_filter_field.insert_char(c);
        }
        self.log_filter_open = true;
    }

    /// Copies the log line the pane is currently anchored on (see
    /// [`LogBuffer::focused_entry`]) to the system clipboard, reporting the outcome as a status
    /// message either way - a clipboard failure is routine (no display server, a locked
    /// clipboard) and must never crash the app.
    fn copy_focused_log_line(&mut self) {
        match self.logs.focused_entry() {
            Some(entry) => {
                let text = entry.to_plain_text();
                match crate::clipboard::copy_to_clipboard(&text) {
                    Ok(()) => self.set_status(StatusSeverity::Ok, "✓ copied log line".to_string()),
                    Err(err) => {
                        self.set_status(StatusSeverity::Error, format!("✗ copy failed: {err}"))
                    }
                }
            }
            None => self.set_status(StatusSeverity::Error, "✗ no log line to copy".to_string()),
        }
    }

    /// The filter applies as it is typed - the log pane narrows live rather than only on
    /// Enter, so a filter that matches nothing is visibly wrong before it's committed.
    fn handle_log_filter_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            // Esc abandons the prompt *and* the filter, matching the roadmap's "`/` to open
            // the filter and `Esc` to clear it".
            KeyCode::Esc => {
                self.log_filter_open = false;
                self.log_filter_field = TextField::default();
                self.logs.clear_filter();
            }
            KeyCode::Enter => self.log_filter_open = false,
            KeyCode::Backspace => {
                self.log_filter_field.backspace();
                self.apply_log_filter();
            }
            KeyCode::Delete => {
                self.log_filter_field.delete();
                self.apply_log_filter();
            }
            KeyCode::Left => self.log_filter_field.move_left(),
            KeyCode::Right => self.log_filter_field.move_right(),
            KeyCode::Home => self.log_filter_field.move_home(),
            KeyCode::End => self.log_filter_field.move_end(),
            KeyCode::Char(c) => {
                self.log_filter_field.insert_char(c);
                self.apply_log_filter();
            }
            _ => {}
        }
    }

    fn apply_log_filter(&mut self) {
        let value = self.log_filter_field.value().to_string();
        if value.is_empty() {
            self.logs.clear_filter();
        } else {
            self.logs.set_filter(value);
        }
    }

    fn handle_help_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc | KeyCode::Char('?') => {
                self.help_open = false;
                self.help_scroll = 0;
            }
            KeyCode::Down | KeyCode::PageDown => self.help_scroll += 1,
            KeyCode::Up | KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(1),
            _ => {}
        }
    }

    fn handle_pick_charger_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Down => self.select_next_charger(),
            KeyCode::Up => self.select_previous_charger(),
            KeyCode::Enter => self.confirm_charger_selection(),
            // Esc clears an active filter first (so it doesn't double as "quit" while
            // narrowing the list); with no filter typed, it opens the quit confirmation.
            KeyCode::Esc => {
                if self.picker_filter.value().is_empty() {
                    self.quit_confirm_open = true;
                } else {
                    self.picker_filter = TextField::default();
                    self.selected_charger = 0;
                }
            }
            KeyCode::Backspace => {
                self.picker_filter.backspace();
                self.selected_charger = 0;
            }
            KeyCode::Char(c) => {
                self.picker_filter.insert_char(c);
                self.selected_charger = 0;
            }
            _ => {}
        }
    }

    fn handle_connection_setup_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => self.return_to_picker(),
            KeyCode::Tab | KeyCode::Down => self.connection_focus_next(),
            KeyCode::BackTab | KeyCode::Up => self.connection_focus_previous(),
            KeyCode::Enter => self.confirm_connection_setup(),
            KeyCode::Char('r') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.connection_password_revealed = !self.connection_password_revealed;
            }
            // Only meaningful on the URL field - there's nothing to cycle from the identity or
            // password fields, so this is a no-op there rather than cycling suggestions out
            // from under a field the user isn't looking at.
            KeyCode::PageDown if self.connection_focused_field == 0 => {
                self.cycle_url_suggestion(true)
            }
            KeyCode::PageUp if self.connection_focused_field == 0 => {
                self.cycle_url_suggestion(false)
            }
            KeyCode::Backspace => {
                self.focused_connection_field_mut().backspace();
                self.note_connection_url_field_edited();
            }
            KeyCode::Delete => {
                self.focused_connection_field_mut().delete();
                self.note_connection_url_field_edited();
            }
            KeyCode::Left => self.focused_connection_field_mut().move_left(),
            KeyCode::Right => self.focused_connection_field_mut().move_right(),
            KeyCode::Home => self.focused_connection_field_mut().move_home(),
            KeyCode::End => self.focused_connection_field_mut().move_end(),
            KeyCode::Char(c) => {
                self.focused_connection_field_mut().insert_char(c);
                self.note_connection_url_field_edited();
            }
            _ => {}
        }
    }

    /// Clears the URL field's error and suggestion-cycle position once it's edited - mirrors
    /// [`Self::cancel_parameter_prompt`]'s "editing clears the complaint" rule. A no-op unless
    /// the URL field (index 0) is the one actually focused, since neither piece of state means
    /// anything for the identity or password fields.
    fn note_connection_url_field_edited(&mut self) {
        if self.connection_focused_field == 0 {
            self.connection_url_error = None;
            self.connection_url_suggestion = None;
        }
    }

    /// The remembered CSMS URLs (see [`ConnectionStore::recent_urls`]) that match what's
    /// currently typed in the URL field, case-insensitively by prefix - the same "typing
    /// narrows the list" rule the picker's charger filter and the command palette's filter
    /// both already follow.
    pub(crate) fn connection_url_suggestions(&self) -> Vec<String> {
        let typed = self.connection_csms_url.value().to_lowercase();
        self.connection_store
            .recent_urls()
            .into_iter()
            .filter(|url| url.to_lowercase().starts_with(&typed))
            .collect()
    }

    /// `PageDown`/`PageUp` on the URL field: moves to the next/previous remembered URL,
    /// wrapping at either end, and fills the field with it. Does nothing if nothing is
    /// remembered yet.
    ///
    /// Deliberately cycles the *full* list from [`ConnectionStore::recent_urls`] rather than
    /// [`Self::connection_url_suggestions`]'s prefix-filtered one: since filling the field with
    /// a suggestion is exactly what this does, using the filtered list would mean the very
    /// first cycle narrows the field's own prefix down to just itself, and every subsequent
    /// press would have nothing left to cycle to.
    fn cycle_url_suggestion(&mut self, forward: bool) {
        let suggestions = self.connection_store.recent_urls();
        if suggestions.is_empty() {
            return;
        }
        let next_index = match self.connection_url_suggestion {
            Some(index) if forward => (index + 1) % suggestions.len(),
            Some(index) => (index + suggestions.len() - 1) % suggestions.len(),
            None if forward => 0,
            None => suggestions.len() - 1,
        };
        self.connection_url_suggestion = Some(next_index);
        self.connection_csms_url = TextField::new(suggestions[next_index].clone());
    }

    fn handle_dashboard_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            // Esc clears an active log filter before it means "leave the dashboard", the same
            // way it clears the picker's filter before it means "quit".
            KeyCode::Esc if self.logs.filter().is_some() => self.logs.clear_filter(),
            KeyCode::Esc => self.return_to_picker(),
            KeyCode::PageUp => self.logs.scroll_up(),
            KeyCode::PageDown => self.logs.scroll_down(),
            KeyCode::Char('/') => self.open_log_filter(),
            KeyCode::Char('g') => self.logs.scroll_to_top(),
            KeyCode::Char('G') => self.logs.scroll_to_bottom(),
            KeyCode::Char('l') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.logs.clear();
                self.set_status(StatusSeverity::Ok, "✓ cleared logs".to_string());
            }
            KeyCode::Char('l') => self.logs.cycle_level_threshold(),
            KeyCode::Char('y') => self.copy_focused_log_line(),
            KeyCode::Down => self.select_next_connector(),
            KeyCode::Up => self.select_previous_connector(),
            KeyCode::Right | KeyCode::Tab => self.select_next_evse(),
            KeyCode::Left | KeyCode::BackTab => self.select_previous_evse(),
            // Ctrl+K is the modern convention for a command palette; 'c' stays as an alias
            // rather than being removed, since it's what this app has always used.
            KeyCode::Char('k') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_command_palette()
            }
            KeyCode::Char('c') => self.open_command_palette(),
            KeyCode::Char('d') => self.toggle_discharging(),
            KeyCode::Char('r') => self.retry_connection(),
            _ => {}
        }
    }

    /// `r`: goes back to the connection setup screen for this charger after a failed dial, with every
    /// field prefilled from the profile that failed (they are remembered in
    /// [`Self::connection_store`]), so retrying is `Enter` and fixing a typo'd URL is an edit.
    ///
    /// Only does anything while a failure is actually showing. This is not a general "reconnect":
    /// a charger that never went through connection setup (1.6J/2.0.1 chargers go straight to the
    /// dashboard, since `connect_charger` is 2.1-only) has no CSMS to retry against, and a charger
    /// that *is* connected must not be torn down by a stray keypress.
    fn retry_connection(&mut self) {
        if self.connection_failure.is_none() {
            return;
        }
        let Some(charger_id) = self
            .charger_state
            .as_ref()
            .map(|state| state.config.id.clone())
        else {
            return;
        };
        // Clears `connection_failure` itself, along with the rest of the previous attempt's state.
        self.enter_connection_setup(&charger_id);
        self.screen = Screen::ConnectionSetup;
    }

    /// `d`: the shortcut for [`HardwareAction::ToggleDischarge`], which is also in the palette.
    ///
    /// The palette only ever lists an action that is available, so it needs no refusal path. A
    /// keybinding does: pressed on a charger that doesn't declare bidirectional power, or with
    /// nothing plugged in, it has to say why rather than doing nothing. Both conditions are
    /// [`HardwareAction::is_available`]'s, asked again here only to explain which one failed - see
    /// its doc comment for why each is a condition at all.
    fn toggle_discharging(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        if !state.config.capabilities.supports_bidirectional_power {
            self.set_status(
                StatusSeverity::Error,
                format!("✗ {} does not declare bidirectional power", state.config.id),
            );
            return;
        }
        let connector = state
            .evses
            .get(self.focused.evse)
            .and_then(|evse| evse.connectors.get(self.focused.connector));
        if !HardwareAction::ToggleDischarge.is_available(state, connector, &self.campaigns) {
            self.set_status(
                StatusSeverity::Error,
                "✗ V2G needs a vehicle plugged in".to_string(),
            );
            return;
        }

        self.run_hardware_action(HardwareAction::ToggleDischarge);
    }

    fn handle_command_palette_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => self.close_command_palette(),
            KeyCode::Down => self.select_next_command(),
            KeyCode::Up => self.select_previous_command(),
            // Retarget without leaving the palette: the target is shown on every row, so
            // noticing it's wrong shouldn't cost an Esc and a re-open. Tab moves the target,
            // not the selection, because the palette's list already moves with ↑/↓.
            KeyCode::Tab => self.retarget_palette(true),
            KeyCode::BackTab => self.retarget_palette(false),
            KeyCode::Enter => self.dispatch_selected_command(),
            KeyCode::Backspace => {
                self.command_palette_filter.backspace();
                self.command_palette_selected = 0;
            }
            KeyCode::Char(c) => {
                self.command_palette_filter.insert_char(c);
                self.command_palette_selected = 0;
            }
            _ => {}
        }
    }

    fn handle_parameter_prompt_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => self.cancel_parameter_prompt(),
            KeyCode::Enter => self.submit_parameter_prompt(),
            // Editing the value clears any complaint about the previous one.
            KeyCode::Backspace => {
                self.parameter_field.backspace();
                self.parameter_error = None;
            }
            KeyCode::Delete => {
                self.parameter_field.delete();
                self.parameter_error = None;
            }
            KeyCode::Left => self.parameter_field.move_left(),
            KeyCode::Right => self.parameter_field.move_right(),
            KeyCode::Home => self.parameter_field.move_home(),
            KeyCode::End => self.parameter_field.move_end(),
            KeyCode::Char(c) => {
                self.parameter_field.insert_char(c);
                self.parameter_error = None;
            }
            _ => {}
        }
    }

    /// [`Self::chargers`] narrowed by the picker's filter text (case-insensitive substring
    /// match on the charger id).
    pub(crate) fn filtered_chargers(&self) -> Vec<&ChargerEntry> {
        let filter = self.picker_filter.value().to_lowercase();
        self.chargers
            .iter()
            .filter(|entry| entry.config.id.to_lowercase().contains(&filter))
            .collect()
    }

    fn select_next_charger(&mut self) {
        let count = self.filtered_chargers().len();
        if count == 0 {
            return;
        }
        if self.selected_charger + 1 < count {
            self.selected_charger += 1;
        }
    }

    fn select_previous_charger(&mut self) {
        self.selected_charger = self.selected_charger.saturating_sub(1);
    }

    /// Every valid `(evse_index, connector_index)` pair in `state`, in display order - EVSEs
    /// top to bottom, each one's connectors beneath it. This is the flattened list
    /// [`Self::select_next_connector`]/[`Self::select_previous_connector`] walk, so `↑`/`↓`
    /// treat the whole tree as one list and flow across EVSE boundaries instead of stopping at
    /// them the way `Tab`/`←`/`→` do.
    fn connector_positions(state: &ChargerState) -> Vec<(usize, usize)> {
        state
            .evses
            .iter()
            .enumerate()
            .flat_map(|(evse_index, evse)| {
                (0..evse.connectors.len()).map(move |connector_index| (evse_index, connector_index))
            })
            .collect()
    }

    /// `↓`: moves focus to the next connector in display order, flowing from the last
    /// connector of one EVSE into the first connector of the next rather than stopping at the
    /// EVSE boundary - see [`Self::connector_positions`]. Clamps at the last connector in the
    /// whole tree; does nothing (rather than panicking) for a charger with no EVSEs, or whose
    /// EVSEs have no connectors.
    fn select_next_connector(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        let positions = Self::connector_positions(state);
        let Some(current) = positions
            .iter()
            .position(|&pos| pos == (self.focused.evse, self.focused.connector))
        else {
            // Focus doesn't point at a real connector (e.g. an EVSE with none) - land on the
            // first one that exists rather than doing nothing.
            if let Some(&(evse, connector)) = positions.first() {
                self.focused = FocusedConnector { evse, connector };
            }
            return;
        };
        if let Some(&(evse, connector)) = positions.get(current + 1) {
            self.focused = FocusedConnector { evse, connector };
        }
    }

    /// `↑`: the mirror image of [`Self::select_next_connector`].
    fn select_previous_connector(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        let positions = Self::connector_positions(state);
        let Some(current) = positions
            .iter()
            .position(|&pos| pos == (self.focused.evse, self.focused.connector))
        else {
            if let Some(&(evse, connector)) = positions.first() {
                self.focused = FocusedConnector { evse, connector };
            }
            return;
        };
        if current > 0
            && let Some(&(evse, connector)) = positions.get(current - 1)
        {
            self.focused = FocusedConnector { evse, connector };
        }
    }

    /// `→`/`Tab`: jumps focus to the next EVSE, always landing on its first connector - unlike
    /// `↑`/`↓` this treats the tree as EVSE-sized steps, matching the pre-Phase-3b behavior
    /// this key retains ("keep working as they do today").
    fn select_next_evse(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        if state.evses.is_empty() {
            return;
        }
        if self.focused.evse + 1 < state.evses.len() {
            self.focused.evse += 1;
        }
        self.focused.connector = 0;
    }

    /// `←`/`BackTab`: the mirror image of [`Self::select_next_evse`].
    fn select_previous_evse(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        if state.evses.is_empty() {
            return;
        }
        self.focused.evse = self.focused.evse.saturating_sub(1);
        self.focused.connector = 0;
    }

    fn open_command_palette(&mut self) {
        if self.charger_state.is_none() {
            return;
        }
        self.command_palette_selected = 0;
        self.command_palette_filter = TextField::default();
        self.command_palette_open = true;
    }

    fn close_command_palette(&mut self) {
        self.command_palette_open = false;
    }

    /// Moves the focused connector while the palette is open, so a command about to be
    /// dispatched can be pointed at a different connector in place.
    ///
    /// The selection is reset because the newly targeted connector may not be eligible for the
    /// same commands - keeping the index would silently land on a different command than the
    /// one the user was looking at.
    fn retarget_palette(&mut self, forward: bool) {
        if forward {
            self.select_next_connector();
        } else {
            self.select_previous_connector();
        }
        self.command_palette_selected = 0;
    }

    /// How the focused connector reads in the palette, e.g. `"EVSE 1 / C1"`. `None` when no
    /// charger is loaded or the focus doesn't point at a real connector.
    pub(crate) fn focused_connector_label(&self) -> Option<String> {
        let state = self.charger_state.as_ref()?;
        let evse = state.evses.get(self.focused.evse)?;
        let connector = evse.connectors.get(self.focused.connector)?;
        Some(format!("EVSE {} / C{}", evse.id, connector.id))
    }

    fn select_next_command(&mut self) {
        let count = self.palette_commands().len();
        if count == 0 {
            return;
        }
        if self.command_palette_selected + 1 < count {
            self.command_palette_selected += 1;
        }
    }

    fn select_previous_command(&mut self) {
        self.command_palette_selected = self.command_palette_selected.saturating_sub(1);
    }

    /// Dispatches the highlighted palette command, or, if it needs a
    /// parameter first (e.g. an RFID tag), closes the palette and opens a
    /// parameter prompt for it instead; the command is actually applied once
    /// that prompt is submitted (see [`submit_parameter_prompt`](Self::submit_parameter_prompt)).
    fn dispatch_selected_command(&mut self) {
        let entries = self.palette_commands();
        let entry = entries.get(self.command_palette_selected).copied();
        self.close_command_palette();

        match entry {
            Some(PaletteEntry::Command(command)) => {
                if let Some(parameter) = command.parameter() {
                    self.open_parameter_prompt(command, parameter);
                    return;
                }
                self.apply_command(command, "");
            }
            // No hardware action takes a parameter, so none of them can reach the prompt path: an
            // install duration or a transfer size is the *charger's* configuration (see
            // `charger_hardware`'s constants), not something to ask for per invocation.
            Some(PaletteEntry::Hardware(action)) => self.run_hardware_action(action),
            None => {}
        }
    }

    /// Sends `action` to the running charger's hardware and reports it, the same way
    /// [`Self::apply_command`] reports a dispatched command - `→`, because what happened is that the
    /// hardware was asked, and the dashboard learns the result from the snapshot that follows.
    ///
    /// Availability was already decided by [`HardwareAction::is_available`] when the palette listed
    /// this, so the checks here are the last-line kind: no charger, no channel, or a focus that
    /// doesn't resolve to a connector.
    fn run_hardware_action(&mut self, action: HardwareAction) {
        let Some(state) = &self.charger_state else {
            return;
        };
        let control = match action {
            HardwareAction::ToggleDischarge => {
                let Some(connector) = state
                    .evses
                    .get(self.focused.evse)
                    .and_then(|evse| evse.connectors.get(self.focused.connector))
                else {
                    return;
                };
                HardwareControl::SetDischarging {
                    evse: self.focused.evse,
                    connector: self.focused.connector,
                    // Read off the last snapshot, never a local flag, so the toggle cannot disagree
                    // with what the sidebar says the hardware is doing.
                    discharging: !connector.discharging,
                }
            }
            HardwareAction::InstallFirmware => HardwareControl::InstallFirmware,
            HardwareAction::UploadDiagnostics => HardwareControl::UploadDiagnostics,
            HardwareAction::FailFirmwareInstall => HardwareControl::FailFirmwareInstall,
            HardwareAction::FailFirmwareDownload => HardwareControl::FailFirmwareDownload,
            HardwareAction::FailDiagnosticsUpload => HardwareControl::FailDiagnosticsUpload,
        };

        let Some(sender) = &self.hardware_control_sender else {
            return;
        };
        let _ = sender.send(control);

        let description = hardware_control_log_line(control, state);
        self.logs.push(description.clone());
        self.set_status(StatusSeverity::Ok, format!("→ {description}"));
    }

    /// Opens the prompt for `command`, prefilled with the last value accepted for the same
    /// parameter. Prefilling rather than merely suggesting means the common case - plugging the
    /// same test vehicle in again - is Enter, not retyping an id.
    fn open_parameter_prompt(&mut self, command: Command, parameter: CommandParameter) {
        let remembered = self
            .parameter_history
            .get(&parameter)
            .cloned()
            .unwrap_or_default();
        self.parameter_field = TextField::new(remembered);
        self.parameter_error = None;
        self.parameter_prompt = Some(command);
    }

    fn cancel_parameter_prompt(&mut self) {
        self.parameter_prompt = None;
        self.parameter_error = None;
    }

    /// Validates the typed value and, if it passes, applies the command and remembers the
    /// value for next time. A rejected value leaves the prompt open with an inline reason
    /// rather than closing and silently doing nothing.
    fn submit_parameter_prompt(&mut self) {
        let Some(command) = self.parameter_prompt else {
            return;
        };
        let input = self.parameter_field.value().trim().to_string();

        if let Some(error) = validate_parameter(input.as_str()) {
            self.parameter_error = Some(error);
            return;
        }

        if let Some(parameter) = command.parameter() {
            self.parameter_history.insert(parameter, input.clone());
        }
        self.parameter_prompt = None;
        self.parameter_error = None;
        self.apply_command(command, &input);
    }

    /// Dispatches `command` against the focused connector (see [`FocusedConnector`]) - never
    /// merely "the focused EVSE's first eligible connector," so a command run from the palette
    /// always acts on the connector actually shown as focused on screen.
    ///
    /// **Always by sending a `ChargePointEvent` to the charger's own state machine**, never by
    /// mutating `ChargerState`: the dashboard picks up the effect when the runtime reports it back
    /// through [`Self::drain_charger_snapshots`]. That holds for a local charger as much as a
    /// CSMS-connected one, since H3b gave both a real `ChargePointRuntime` - `charger.rs`'s coarse
    /// `Command::apply_to` is no longer on any path the TUI takes. It used to be the fallback here
    /// whenever `live_ocpp_state` was still `None`, which made the first frame or two after
    /// selecting a charger quietly dishonest: the mutation looked like it worked and was then
    /// reverted by the first snapshot to arrive. That window now reports "not ready yet" instead.
    ///
    /// Display commands are the exception, and the only state the TUI still writes itself:
    /// `ChargerState::display_message` has no projection behind it, because `ChargerHardware`'s
    /// `FakeDisplay` is moved into the builder's registration and no handle survives for a snapshot
    /// to read (`FakeDisplay` isn't `Clone` the way `FakeChargePoint` is) - the same handle problem
    /// H7 had, still open for the display. So `SetDisplayMessage`/`ClearDisplayMessage` apply
    /// locally, live CSMS connection or not, and a message a *CSMS* sets lands on the hardware where
    /// the dashboard cannot see it.
    fn apply_command(&mut self, command: Command, input: &str) {
        if command.is_display_command() {
            if let Some(state) = &mut self.charger_state
                && let Some(log_line) = command.apply_to_charger(state, input)
            {
                self.logs.push(log_line);
                self.set_status(StatusSeverity::Ok, format!("✓ {}", command.label()));
            }
            return;
        }

        let Some(sender) = &self.ocpp_event_sender else {
            // No charger thread is running at all, so there is nothing to dispatch against.
            return;
        };
        let Some(ocpp_state) = &self.live_ocpp_state else {
            // A thread exists but hasn't published its first snapshot yet - a window of a frame or
            // two after selecting a charger. This used to fall back to `Command::apply_to`,
            // mutating `ChargerState` directly, which *looked* like it worked and was then silently
            // reverted by the first snapshot to arrive (`apply_ocpp_state` overwrites every
            // connector's status from the real protocol state). Saying "not ready yet" is the
            // honest answer, and the same one an ineligible connector already gets.
            self.set_status(
                StatusSeverity::Error,
                format!("✗ {} not ready yet", command.label()),
            );
            return;
        };

        match build_ocpp_event_for_connector(
            ocpp_state,
            self.focused.evse,
            self.focused.connector,
            command,
            input,
        ) {
            Some(event) => {
                let _ = sender.send(event);
                self.logs.push(format!("{} sent to CSMS", command.label()));
                self.set_status(StatusSeverity::Ok, format!("→ {}", command.label()));
            }
            None => {
                self.set_status(
                    StatusSeverity::Error,
                    format!("✗ {} not ready yet", command.label()),
                );
            }
        }
    }

    fn confirm_charger_selection(&mut self) {
        if let Some(charger) = self
            .filtered_chargers()
            .get(self.selected_charger)
            .map(|entry| (*entry).clone())
        {
            self.logs = LogBuffer::default();
            self.logs.push(format!("{} booting", charger.config.id));
            self.charger_state = Some(ChargerState::from_config(charger.config.clone()));
            self.focused = FocusedConnector::default();
            self.status_message = None;
            // Reset so the first tick on the new charger sees zero elapsed time instead of
            // however long was spent idling on the picker.
            self.last_metrics_tick = None;

            if charger.config.ocpp_version == OcppVersion::V21 {
                self.enter_connection_setup(&charger.config.id);
                self.screen = Screen::ConnectionSetup;
            } else {
                // 1.6J/2.0.1 chargers never go through the connection setup screen at all - see
                // `selecting_a_1_6j_or_2_0_1_charger_still_goes_straight_to_the_dashboard` - so
                // this is the only place their (always-local; `connect_charger` is 2.1-only)
                // runtime ever gets started.
                self.spawn_local_charger(charger.config);
                self.screen = Screen::Dashboard;
            }
        }
    }

    /// Starts `config` running locally - no CSMS, no dial, no registration - on a dedicated
    /// background thread built the same way [`Self::confirm_connection_setup`]'s connected path
    /// is, wiring up the same state/command/tick channels (see [`drive_running_charger`]).
    ///
    /// A dedicated OS thread with its own single-threaded runtime isn't strictly required here
    /// the way it is for [`connect_charger`] (whose future isn't `Send` - see
    /// `confirm_connection_setup`'s own comment); [`start_local_charger`]'s future is `Send`; a
    /// `tokio::spawn` onto the app's own runtime would work too. Using the identical thread shape
    /// anyway is deliberate: `docs/hardware-roadmap.md`'s H3b is about a local and a connected
    /// charger being driven the same way, and that includes the TUI-side plumbing, not only the
    /// `core` types underneath it.
    ///
    /// Gives `start_local_charger` the same real [`ChargerHardware`] bundle
    /// [`Self::confirm_connection_setup`]'s connected path builds, rooted at the same per-charger
    /// state directory (`docs/hardware-roadmap.md` decision 6: a local charger is the one people
    /// leave running, so it persists by default too - see the README's "State and persistence"
    /// section for the cost that accepts). Whether that bundle actually touches disk still depends
    /// entirely on the charger's own declared `capabilities.has_persistent_storage`/`has_display` -
    /// `ChargerHardware::new` only supplies the storage/display *objects*, and `register_setup_blocks`
    /// (H5b/H6b) never reads from or writes to either unless the matching capability says to.
    fn spawn_local_charger(&mut self, config: ChargerConfig) {
        let (snapshot_sender, snapshot_receiver) = mpsc::unbounded_channel();
        self.charger_snapshot_receiver = Some(snapshot_receiver);

        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        self.ocpp_event_sender = Some(event_sender);

        let (tick_sender, tick_receiver) = mpsc::unbounded_channel();
        self.ocpp_tick_sender = Some(tick_sender);

        let (control_sender, control_receiver) = mpsc::unbounded_channel();
        self.hardware_control_sender = Some(control_sender);

        std::thread::spawn(move || {
            let tokio_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build a runtime for the local charger");
            tokio_runtime.block_on(async move {
                let hardware = charger_hardware(&config);
                let campaigns = CampaignHandles::of(&hardware);
                let running = start_local_charger(&config, hardware).await;
                drive_running_charger(
                    running,
                    snapshot_sender,
                    event_receiver,
                    tick_receiver,
                    control_receiver,
                    campaigns,
                )
                .await;
            });
        });
    }

    /// Prefills the connection setup fields from the last-remembered profile for
    /// `charger_id`, or sensible blanks (identity defaulting to the charger id) if
    /// there isn't one yet.
    fn enter_connection_setup(&mut self, charger_id: &str) {
        match self.connection_store.get(charger_id).cloned() {
            Some(profile) => {
                let SecurityProfile::Basic { password } = profile.security;
                self.connection_csms_url = TextField::new(profile.csms_url);
                self.connection_ocpp_identity = TextField::new(profile.ocpp_identity);
                self.connection_password = TextField::new(password)
                    .with_max_bytes(SecurityProfile::MAX_BASIC_PASSWORD_BYTES);
            }
            None => {
                self.connection_csms_url = TextField::default();
                self.connection_ocpp_identity = TextField::new(charger_id);
                self.connection_password =
                    TextField::default().with_max_bytes(SecurityProfile::MAX_BASIC_PASSWORD_BYTES);
            }
        }
        self.connection_focused_field = 0;
        // A new attempt is being set up, so the previous one's failure has been acted on.
        self.connection_failure = None;
        self.connection_url_error = None;
        self.connection_password_revealed = false;
        self.connection_url_suggestion = None;
    }

    fn connection_focus_next(&mut self) {
        if self.connection_focused_field + 1 < 3 {
            self.connection_focused_field += 1;
        }
    }

    fn connection_focus_previous(&mut self) {
        self.connection_focused_field = self.connection_focused_field.saturating_sub(1);
    }

    fn focused_connection_field_mut(&mut self) -> &mut TextField {
        match self.connection_focused_field {
            0 => &mut self.connection_csms_url,
            1 => &mut self.connection_ocpp_identity,
            _ => &mut self.connection_password,
        }
    }

    fn confirm_connection_setup(&mut self) {
        if let Some(error) = validate_csms_url(self.connection_csms_url.value()) {
            self.connection_url_error = Some(error);
            return;
        }
        self.connection_url_error = None;

        let Some(state) = &self.charger_state else {
            return;
        };
        let charger_id = state.config.id.clone();
        let config = state.config.clone();

        let profile = ConnectionProfile {
            csms_url: self.connection_csms_url.value().to_string(),
            ocpp_identity: self.connection_ocpp_identity.value().to_string(),
            security: SecurityProfile::Basic {
                password: self.connection_password.value().to_string(),
            },
        };

        self.connection_store.remember(charger_id, profile.clone());
        if let Some(path) = &self.connection_store_path
            && let Err(error) = self.connection_store.save(path)
        {
            tracing::warn!(%error, "failed to save connection store");
        }

        // Decide the simulation mode right here, before anything below spawns the connection
        // thread that would start feeding it live state snapshots - see
        // `resolve_simulation_mode`'s doc comment for why the ordering matters.
        if let Some(state) = &mut self.charger_state {
            state.mode = resolve_simulation_mode(&profile.csms_url);
        }

        self.screen = Screen::Dashboard;

        if profile.csms_url.trim().is_empty() {
            // Local was chosen on the connection setup screen itself - same runtime, same
            // channels, same background-thread shape as a 1.6J/2.0.1 charger going straight to
            // the dashboard (see [`Self::spawn_local_charger`]), just reached from here instead.
            self.spawn_local_charger(config);
            return;
        }

        let (result_sender, result_receiver) = oneshot::channel();
        self.connect_result_receiver = Some(result_receiver);

        let (snapshot_sender, snapshot_receiver) = mpsc::unbounded_channel();
        self.charger_snapshot_receiver = Some(snapshot_receiver);

        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        self.ocpp_event_sender = Some(event_sender);

        let (tick_sender, tick_receiver) = mpsc::unbounded_channel();
        self.ocpp_tick_sender = Some(tick_sender);

        let (control_sender, control_receiver) = mpsc::unbounded_channel();
        self.hardware_control_sender = Some(control_sender);

        // `connect_and_setup`'s future isn't `Send` (upstream uses non-Send sync
        // primitives internally), so it can't go through `tokio::spawn`. A dedicated
        // thread with its own single-threaded runtime sidesteps that: `block_on`
        // doesn't require `Send`. Unlike a one-shot connect attempt, this thread
        // outlives the initial handshake: once connected, it forwards every live state
        // snapshot, every dispatched command and every simulated-time tick for as long as the
        // App's receiver/sender ends of these channels stay alive (dropped in
        // `return_to_picker`, which ends every loop in `drive_running_charger` and lets the
        // thread exit).
        std::thread::spawn(move || {
            let tokio_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build a runtime for the CSMS connection attempt");
            tokio_runtime.block_on(async move {
                let hardware = charger_hardware(&config);
                let campaigns = CampaignHandles::of(&hardware);
                match connect_charger(&config, &profile, hardware).await {
                    Ok(running) => {
                        let _ = result_sender.send(Ok(()));
                        drive_running_charger(
                            running,
                            snapshot_sender,
                            event_receiver,
                            tick_receiver,
                            control_receiver,
                            campaigns,
                        )
                        .await;
                    }
                    Err(error) => {
                        let _ = result_sender.send(Err(error.to_string()));
                    }
                }
            });
        });
    }

    /// Checks whether a background CSMS connection attempt has resolved, and if so
    /// surfaces the outcome as a status message.
    fn poll_connect_result(&mut self) {
        let Some(receiver) = &mut self.connect_result_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(())) => {
                self.set_status(StatusSeverity::Ok, "✓ connected to CSMS".to_string());
                self.connection_failure = None;
                self.connect_result_receiver = None;
            }
            Ok(Err(error)) => {
                // Three places, each doing something the others can't: the toast is the immediate
                // "that just happened", the log entry puts it in the trace with everything else that
                // led up to it, and `connection_failure` is what is still on screen a minute later.
                self.set_status(
                    StatusSeverity::Error,
                    format!("✗ CSMS connection failed: {error}"),
                );
                self.logs
                    .push(LogEntry::error(format!("CSMS connection failed: {error}")));
                self.connection_failure = Some(error);
                self.connect_result_receiver = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.connect_result_receiver = None;
            }
        }
    }

    /// Drops the live-connection channels (if any), which ends the background connection
    /// thread's forwarding loops and lets it exit, rather than leaving it running against a
    /// charger that's no longer shown.
    ///
    /// This also drops `charger_state` itself, which incidentally takes any `SimulationMode`
    /// with it: the next charger picked always starts from a fresh
    /// `ChargerState::from_config`, which defaults to `SimulationMode::Local`. There's nothing
    /// further to reset here, but it's worth spelling out - a future refactor that made
    /// `charger_state` persist across selections would need to explicitly reset `mode` too.
    fn return_to_picker(&mut self) {
        self.charger_state = None;
        self.status_message = None;
        self.charger_snapshot_receiver = None;
        self.ocpp_event_sender = None;
        self.ocpp_tick_sender = None;
        self.hardware_control_sender = None;
        self.connection_failure = None;
        self.campaigns = CampaignProgress::default();
        self.live_ocpp_state = None;
        self.screen = Screen::PickCharger;
    }

    /// Pulls every entry currently buffered in the tracing bridge's channel
    /// (if one is installed) into the log panel.
    fn drain_log_receiver(&mut self) {
        let Some(receiver) = &mut self.log_receiver else {
            return;
        };
        while let Ok(entry) = receiver.try_recv() {
            self.logs.push(entry);
        }
    }

    /// Pulls every [`ChargerSnapshot`] forwarded from the running charger's background thread,
    /// applying both halves of each to the dashboard's display state and remembering the latest
    /// protocol state for [`Self::apply_command`] to dispatch against.
    ///
    /// Applying both halves here, in this order, is exactly what `RunningCharger::apply_state` does
    /// on the charger's own thread - see [`ChargerSnapshot`] for why the TUI can't just call that.
    fn drain_charger_snapshots(&mut self) {
        let Some(receiver) = &mut self.charger_snapshot_receiver else {
            return;
        };
        while let Ok(snapshot) = receiver.try_recv() {
            if let Some(charger_state) = &mut self.charger_state {
                apply_ocpp_state(charger_state, &snapshot.ocpp);
                apply_hardware_snapshot(charger_state, &snapshot.hardware);
            }
            self.campaigns = snapshot.campaigns;
            self.live_ocpp_state = Some(snapshot.ocpp);
        }
    }

    /// Advances the focused charger's session/SoC bookkeeping (see [`ChargerState::tick`]) by the
    /// real time elapsed since the last call (not a fixed per-frame amount, since the main
    /// loop's cadence varies with input activity), and forwards that same `elapsed` to the
    /// charger's background thread so it can call `RunningCharger::tick` - the only thing that
    /// actually advances the meter, for a local charger and a live-CSMS one alike (H3b).
    fn tick_metrics(&mut self) {
        let now = Instant::now();
        let elapsed = self
            .last_metrics_tick
            .map(|last| now.duration_since(last))
            .unwrap_or_default();
        self.last_metrics_tick = Some(now);
        self.tick_metrics_with(elapsed, now);
    }

    fn tick_metrics_with(&mut self, elapsed: Duration, now: Instant) {
        if let Some(state) = &mut self.charger_state {
            state.tick(elapsed);
        }
        self.expire_status_message(now);
        if let Some(sender) = &self.ocpp_tick_sender {
            let _ = sender.send(elapsed);
        }
    }

    /// Shows `message` in the command bar, stamped so it expires. Always go through this
    /// rather than assigning `status_message` directly - an un-stamped toast would never
    /// expire, which is the bug this replaced.
    pub(crate) fn set_status(&mut self, severity: StatusSeverity, message: String) {
        self.status_message = Some(Toast {
            severity,
            message,
            shown_at: Instant::now(),
        });
    }

    fn expire_status_message(&mut self, now: Instant) {
        if let Some(toast) = &self.status_message
            && now.duration_since(toast.shown_at) >= STATUS_MESSAGE_TTL
        {
            self.status_message = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::LogLevel;
    use charge_point_simulator_core::charger::{
        CapabilitiesConfig, ChargerConfig, ChargerSource, ConnectionStatus, ConnectorStatus,
        EvseConfig, OcppVersion,
    };
    use ocpp_charge_point::state::{
        ConnectorEvent, ConnectorState as OcppConnectorState, EvseEvent, RegistrationStatus,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// The command bar's current message, without the `Instant` a `Toast` also carries -
    /// assertions care about severity and text, never when it was shown.
    fn status(app: &App) -> Option<(StatusSeverity, String)> {
        app.status_message
            .as_ref()
            .map(|toast| (toast.severity, toast.message.clone()))
    }

    /// The visible log pane as plain strings. Most assertions here care about *which* lines
    /// are shown, not the structure Phase 4 gave each entry.
    fn log_messages(logs: &LogBuffer) -> Vec<String> {
        logs.visible_lines(10)
            .into_iter()
            .map(|entry| entry.message.clone())
            .collect()
    }

    fn charger(id: &str) -> ChargerEntry {
        charger_with_evses(
            id,
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        )
    }

    fn charger_with_evses(id: &str, evses: Vec<EvseConfig>) -> ChargerEntry {
        ChargerEntry {
            config: ChargerConfig {
                id: id.into(),
                ocpp_version: OcppVersion::V16J,
                evses,
                has_display: false,
                capabilities: Default::default(),
            },
            source: ChargerSource::BuiltIn,
        }
    }

    fn charger_v21(id: &str) -> ChargerEntry {
        ChargerEntry {
            config: ChargerConfig {
                id: id.into(),
                ocpp_version: OcppVersion::V21,
                evses: vec![EvseConfig {
                    id: 1,
                    connectors: 1,
                }],
                has_display: false,
                capabilities: Default::default(),
            },
            source: ChargerSource::BuiltIn,
        }
    }

    fn charger_with_display(id: &str) -> ChargerEntry {
        ChargerEntry {
            config: ChargerConfig {
                id: id.into(),
                ocpp_version: OcppVersion::V16J,
                evses: vec![EvseConfig {
                    id: 1,
                    connectors: 1,
                }],
                has_display: true,
                capabilities: Default::default(),
            },
            source: ChargerSource::BuiltIn,
        }
    }

    #[test]
    fn starts_on_the_charger_picker() {
        let app = App::new(vec![charger("CP001")]);
        assert_eq!(app.screen, Screen::PickCharger);
        assert_eq!(app.selected_charger, 0);
        assert!(app.charger_state.is_none());
    }

    #[test]
    fn q_opens_a_quit_confirmation_instead_of_exiting_immediately() {
        // On the picker, 'q' is typeable into the charger filter instead - Esc (with the
        // filter empty) is how the picker opens the quit confirmation.
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.quit_confirm_open);
        assert!(!app.exit);

        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('q')));
        assert!(app.quit_confirm_open);
        assert!(!app.exit);
    }

    #[test]
    fn y_or_enter_confirms_the_quit() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Char('y')));
        assert!(app.exit);

        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Enter));
        assert!(app.exit);
    }

    #[test]
    fn n_or_esc_cancels_the_quit() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Char('n')));
        assert!(!app.quit_confirm_open);
        assert!(!app.exit);

        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.quit_confirm_open);
        assert!(!app.exit);
    }

    #[test]
    fn question_mark_toggles_the_help_overlay() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('?')));
        assert!(app.help_open);

        app.handle_key_event(key(KeyCode::Char('?')));
        assert!(!app.help_open);
    }

    #[test]
    fn esc_closes_the_help_overlay() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('?')));
        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.help_open);
    }

    #[test]
    fn keys_are_swallowed_by_the_help_overlay_while_it_is_open() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.handle_key_event(key(KeyCode::Char('?')));
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected_charger, 0);
    }

    #[test]
    fn down_moves_selection_and_clamps_at_the_end() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected_charger, 1);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected_charger, 1);
    }

    #[test]
    fn up_moves_selection_and_clamps_at_the_start() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.selected_charger = 1;
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.selected_charger, 0);
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.selected_charger, 0);
    }

    #[test]
    fn navigation_on_an_empty_charger_list_does_not_panic() {
        let mut app = App::new(vec![]);
        app.handle_key_event(key(KeyCode::Down));
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.selected_charger, 0);
    }

    #[test]
    fn enter_selects_the_highlighted_charger_and_opens_the_dashboard() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.handle_key_event(key(KeyCode::Down));
        app.handle_key_event(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Dashboard);
        assert_eq!(app.charger_state.unwrap().config.id, "CP002");
    }

    #[test]
    fn typing_in_the_picker_filters_by_charger_id_and_resets_the_selection() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected_charger, 1);

        for c in "cp002".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }

        let ids: Vec<&str> = app
            .filtered_chargers()
            .iter()
            .map(|e| e.config.id.as_str())
            .collect();
        assert_eq!(ids, vec!["CP002"]);
        assert_eq!(app.selected_charger, 0);
    }

    #[test]
    fn enter_selects_the_highlighted_charger_from_the_filtered_list() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        for c in "cp002".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }

        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.charger_state.unwrap().config.id, "CP002");
    }

    #[test]
    fn esc_clears_the_filter_before_opening_the_quit_confirmation() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('x')));
        assert_eq!(app.picker_filter.value(), "x");

        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.picker_filter.value(), "");
        assert!(!app.quit_confirm_open);

        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.quit_confirm_open);
    }

    #[test]
    fn q_is_typed_into_the_picker_filter_instead_of_opening_quit_confirm() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('q')));

        assert_eq!(app.picker_filter.value(), "q");
        assert!(!app.quit_confirm_open);
    }

    #[test]
    fn question_mark_still_opens_help_from_the_picker() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('?')));

        assert!(app.help_open);
        assert_eq!(app.picker_filter.value(), "");
    }

    #[test]
    fn enter_with_no_chargers_available_does_nothing() {
        let mut app = App::new(vec![]);
        app.handle_key_event(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::PickCharger);
        assert!(app.charger_state.is_none());
    }

    #[test]
    fn confirming_a_selection_seeds_a_fresh_booting_charger_state() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        let state = app.charger_state.unwrap();
        assert_eq!(state.connection_status, ConnectionStatus::Booting);
        assert_eq!(state.evses.len(), 1);
        assert_eq!(state.evses[0].connectors.len(), 1);
    }

    #[test]
    fn confirming_a_selection_logs_a_boot_message() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        assert_eq!(log_messages(&app.logs), vec!["CP001 booting"]);
    }

    /// End-to-end proof of the bug `CLAUDE.md` used to record and `docs/hardware-roadmap.md`'s
    /// H3b fixes: a locally-driven charger's `connection_status` no longer reports `Booting`
    /// forever. `confirm_charger_selection` really does spawn a background thread running a real
    /// `RunningCharger` (see `spawn_local_charger`) - unlike every other test in this module,
    /// this one lets that thread actually run rather than injecting `charger_snapshot_receiver`/
    /// `ocpp_tick_sender` by hand, so it is deliberately the one place this suite waits on real
    /// (if very short-lived) background-thread timing. The thread does no I/O: `charger()` below
    /// builds a config with default (all-`false`) capabilities, and `ChargerHardware::new`'s real
    /// `FileStorage`/`FakeDisplay` (decision 6) are only ever read from or written to when the
    /// charger's own `has_persistent_storage`/`has_display` capability says to - see
    /// `register_setup_blocks`'s own doc comment - so this thread only starts the fake hardware
    /// and an authorization worker, exactly as before. Polling is bounded well under what would
    /// ever be a flake risk in practice; a genuine regression (the thread never sending anything,
    /// or `apply_ocpp_state` never reaching `Offline`) fails this test rather than hanging it.
    #[test]
    fn a_locally_selected_charger_reports_offline_rather_than_booting_forever() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        assert_eq!(
            app.charger_state.as_ref().unwrap().connection_status,
            ConnectionStatus::Booting,
            "the freshly seeded state reads Booting until the background thread's first \
             snapshot is drained"
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            app.drain_charger_snapshots();
            if app.charger_state.as_ref().unwrap().connection_status == ConnectionStatus::Offline {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "never received a snapshot from the local charger's background thread"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn confirming_a_selection_focuses_the_first_connector_of_the_first_evse() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 1,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 0,
                connector: 0
            }
        );
    }

    #[test]
    fn right_and_tab_move_focus_to_the_next_evse_and_clamp_at_the_end() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 1,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Right));
        assert_eq!(app.focused.evse, 1);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.focused.evse, 1);
    }

    #[test]
    fn left_and_backtab_move_focus_to_the_previous_evse_and_clamp_at_the_start() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 1,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused.evse = 1;

        app.handle_key_event(key(KeyCode::Left));
        assert_eq!(app.focused.evse, 0);
        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(app.focused.evse, 0);
    }

    #[test]
    fn jumping_evse_with_tab_resets_focus_to_that_evses_first_connector() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 2,
                },
                EvseConfig {
                    id: 2,
                    connectors: 2,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };

        app.handle_key_event(key(KeyCode::Tab));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 1,
                connector: 0
            }
        );
    }

    #[test]
    fn evse_focus_navigation_on_a_charger_with_no_evses_does_not_panic() {
        let mut app = App::new(vec![charger_with_evses("CP001", vec![])]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Right));
        app.handle_key_event(key(KeyCode::Left));
        assert_eq!(app.focused, FocusedConnector::default());
    }

    #[test]
    fn down_moves_focus_across_connectors_within_one_evse() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
        )]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 0,
                connector: 1
            }
        );
    }

    #[test]
    fn down_flows_from_the_last_connector_of_one_evse_into_the_first_of_the_next() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 2,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };

        app.handle_key_event(key(KeyCode::Down));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 1,
                connector: 0
            }
        );
    }

    #[test]
    fn down_clamps_at_the_very_last_connector_of_the_whole_tree() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 1,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused = FocusedConnector {
            evse: 1,
            connector: 0,
        };

        app.handle_key_event(key(KeyCode::Down));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 1,
                connector: 0
            }
        );
    }

    #[test]
    fn up_flows_from_the_first_connector_of_one_evse_into_the_last_of_the_previous() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 2,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused = FocusedConnector {
            evse: 1,
            connector: 0,
        };

        app.handle_key_event(key(KeyCode::Up));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 0,
                connector: 1
            }
        );
    }

    #[test]
    fn up_clamps_at_the_very_first_connector_of_the_whole_tree() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        )]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Up));

        assert_eq!(app.focused, FocusedConnector::default());
    }

    #[test]
    fn up_and_down_navigation_on_a_charger_with_no_evses_does_not_panic() {
        let mut app = App::new(vec![charger_with_evses("CP001", vec![])]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Down));
        app.handle_key_event(key(KeyCode::Up));

        assert_eq!(app.focused, FocusedConnector::default());
    }

    #[test]
    fn up_and_down_navigation_on_an_evse_with_no_connectors_does_not_panic() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![EvseConfig {
                id: 1,
                connectors: 0,
            }],
        )]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Down));
        app.handle_key_event(key(KeyCode::Up));

        assert_eq!(app.focused, FocusedConnector::default());
    }

    /// Every dispatch goes to the charger's own state machine as a `ChargePointEvent`, so what this
    /// asserts is that the event names the focused connector - a command silently acting on a
    /// different connector than the one on screen is the bug the per-connector API exists to prevent.
    #[test]
    fn apply_command_acts_on_the_specifically_focused_connector_not_just_the_first_eligible_one() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        let mut ocpp = ChargePointState::new([2]);
        ocpp.registration = Some(RegistrationStatus::Accepted);
        app.live_ocpp_state = Some(ocpp);
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);
        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };

        app.apply_command(Command::PlugInVehicle, "MY-EV-2");

        assert_eq!(
            receiver.try_recv().unwrap(),
            ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 1,
                    event: ConnectorEvent::CableConnected,
                },
            }
        );
    }

    /// The window this covers used to be a real (if brief) lie: between selecting a charger and its
    /// thread's first snapshot, a dispatched command mutated `ChargerState` directly, appeared to
    /// work, and was then silently reverted by `apply_ocpp_state` the moment that snapshot landed.
    #[test]
    fn a_command_dispatched_before_the_first_snapshot_says_so_rather_than_faking_it() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);
        assert!(
            app.live_ocpp_state.is_none(),
            "no snapshot has been drained yet"
        );

        app.apply_command(Command::PlugInVehicle, "MY-EV-1");

        assert!(receiver.try_recv().is_err(), "nothing to dispatch against");
        assert_eq!(
            status(&app),
            Some((
                StatusSeverity::Error,
                "✗ Plug in vehicle not ready yet".to_string()
            ))
        );
        assert_eq!(
            app.charger_state.as_ref().unwrap().evses[0].connectors[0].vehicle,
            None,
            "and nothing was mutated locally to be reverted a frame later"
        );
    }

    #[test]
    fn available_commands_reflect_the_specifically_focused_connector() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
        )]);
        app.confirm_charger_selection();
        // Occupy connector 1 (index 0) so it no longer offers "Plug in vehicle"; connector 2
        // (index 1) stays Available and offers it.
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Occupied;

        app.focused = FocusedConnector {
            evse: 0,
            connector: 0,
        };
        let labels_for_connector_1: Vec<&str> =
            app.available_commands().iter().map(|c| c.label()).collect();
        assert!(!labels_for_connector_1.contains(&"Plug in vehicle"));
        assert!(labels_for_connector_1.contains(&"Present RFID card"));

        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };
        let labels_for_connector_2: Vec<&str> =
            app.available_commands().iter().map(|c| c.label()).collect();
        assert!(labels_for_connector_2.contains(&"Plug in vehicle"));
        assert!(!labels_for_connector_2.contains(&"Present RFID card"));
    }

    #[test]
    fn c_opens_the_command_palette_on_the_first_command() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Char('c')));
        assert!(app.command_palette_open);
        assert_eq!(app.command_palette_selected, 0);
    }

    #[test]
    fn c_does_nothing_without_a_selected_charger() {
        let mut app = App::new(vec![]);
        app.handle_key_event(key(KeyCode::Char('c')));
        assert!(!app.command_palette_open);
    }

    #[test]
    fn a_charger_without_a_display_never_offers_display_commands() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        let labels: Vec<&str> = app.available_commands().iter().map(|c| c.label()).collect();
        assert!(!labels.contains(&"Set display message"));
        assert!(!labels.contains(&"Clear display message"));
    }

    #[test]
    fn a_charger_with_a_display_offers_set_but_not_clear_until_a_message_is_showing() {
        let mut app = App::new(vec![charger_with_display("CP-display")]);
        app.confirm_charger_selection();

        let labels: Vec<&str> = app.available_commands().iter().map(|c| c.label()).collect();
        assert!(labels.contains(&"Set display message"));
        assert!(!labels.contains(&"Clear display message"));
    }

    #[test]
    fn setting_a_display_message_shows_it_and_then_offers_clear() {
        let mut app = App::new(vec![charger_with_display("CP-display")]);
        app.confirm_charger_selection();

        app.apply_command(Command::SetDisplayMessage, "Welcome to Flowion");

        assert_eq!(
            app.charger_state.as_ref().unwrap().display_message,
            Some("Welcome to Flowion".to_string())
        );
        assert_eq!(
            status(&app),
            Some((StatusSeverity::Ok, "✓ Set display message".to_string()))
        );
        let labels: Vec<&str> = app.available_commands().iter().map(|c| c.label()).collect();
        assert!(labels.contains(&"Clear display message"));
    }

    #[test]
    fn clearing_a_display_message_blanks_it() {
        let mut app = App::new(vec![charger_with_display("CP-display")]);
        app.confirm_charger_selection();
        app.apply_command(Command::SetDisplayMessage, "hello");

        app.apply_command(Command::ClearDisplayMessage, "");

        assert_eq!(app.charger_state.as_ref().unwrap().display_message, None);
    }

    #[test]
    fn display_commands_apply_locally_even_when_a_live_csms_sender_is_present() {
        let mut app = App::new(vec![charger_with_display("CP-display")]);
        app.confirm_charger_selection();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);

        app.apply_command(Command::SetDisplayMessage, "hello");

        assert_eq!(
            app.charger_state.as_ref().unwrap().display_message,
            Some("hello".to_string())
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn esc_closes_the_command_palette_without_dispatching() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));

        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.command_palette_open);
        // still booting, no vehicle plugged in - nothing was dispatched
        assert_eq!(
            app.charger_state.unwrap().evses[0].connectors[0].vehicle,
            None
        );
    }

    #[test]
    fn typing_in_the_command_palette_filters_by_label_and_resets_the_selection() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));

        // a fresh connector offers "Plug in vehicle" and "Report fault"
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.command_palette_selected, 1);

        for c in "fault".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }

        let labels: Vec<&str> = app.palette_commands().iter().map(|c| c.label()).collect();
        assert_eq!(labels, vec!["Report fault"]);
        assert_eq!(app.command_palette_selected, 0);
    }

    #[test]
    fn backspacing_the_command_palette_filter_restores_hidden_commands() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));

        for c in "fault".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        for _ in 0..5 {
            app.handle_key_event(key(KeyCode::Backspace));
        }

        assert_eq!(app.palette_commands().len(), app.available_commands().len());
    }

    #[test]
    fn selecting_a_command_that_needs_a_parameter_opens_a_prompt_instead_of_dispatching() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // highlighted command is "Plug in vehicle"

        assert!(!app.command_palette_open);
        assert_eq!(app.parameter_prompt, Some(Command::PlugInVehicle));
        // nothing applied yet
        assert_eq!(
            app.charger_state.unwrap().evses[0].connectors[0].vehicle,
            None
        );
    }

    /// A dashboard whose charger has already published a snapshot, with the event channel stubbed so
    /// dispatch can be observed: the shape every command dispatch takes in the running app, since
    /// `apply_command` sends a `ChargePointEvent` and never touches `ChargerState` itself.
    ///
    /// `connector` is the OCPP state the connector is in, which decides both which commands are
    /// offered and which event each maps to.
    fn dispatchable_app(
        connector: OcppConnectorState,
    ) -> (App, UnboundedReceiver<ChargePointEvent>) {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        // One snapshot drained, so `ChargerState` (which decides what the palette offers) and
        // `live_ocpp_state` (which decides what each command maps to) agree - exactly what the
        // running app's first frame does, and what makes the two halves consistent here.
        let (snapshot_sender, snapshot_receiver) = mpsc::unbounded_channel();
        app.charger_snapshot_receiver = Some(snapshot_receiver);
        snapshot_sender
            .send(ChargerSnapshot {
                ocpp: ocpp_state_with(connector),
                hardware: vec![vec![ConnectorHardwareSnapshot::default()]],
                campaigns: CampaignProgress::default(),
            })
            .expect("the receiver is alive");
        app.drain_charger_snapshots();

        let (sender, receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);
        (app, receiver)
    }

    #[test]
    fn submitting_the_parameter_prompt_applies_the_command_with_the_given_input() {
        // `Locked` rather than `Available`, so the command offered is "Present RFID card" - whose
        // parameter (the tag) actually reaches the dispatched event, which is what "with the given
        // input" is about.
        let (mut app, mut receiver) = dispatchable_app(OcppConnectorState::Locked);
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the "RFID tag" prompt

        for c in "TAG-42".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(key(KeyCode::Enter));

        assert!(app.parameter_prompt.is_none());
        let sent = receiver.try_recv().unwrap();
        let ChargePointEvent::Evse {
            event:
                EvseEvent::Connector {
                    event: ConnectorEvent::IdTokenPresented(token),
                    ..
                },
            ..
        } = sent
        else {
            panic!("expected an IdTokenPresented event, got {sent:?}");
        };
        assert_eq!(token.value, "TAG-42");
    }

    #[test]
    fn esc_cancels_the_parameter_prompt_without_applying_the_command() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the prompt

        app.handle_key_event(key(KeyCode::Esc));

        assert!(app.parameter_prompt.is_none());
        assert_eq!(
            app.charger_state.unwrap().evses[0].connectors[0].vehicle,
            None
        );
    }

    #[test]
    fn down_and_up_move_the_command_palette_selection_and_clamp() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));

        // a fresh connector only has "Plug in vehicle" and "Report fault" available
        assert_eq!(app.available_commands().len(), 2);

        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.command_palette_selected, 1);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.command_palette_selected, 1);

        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.command_palette_selected, 0);
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.command_palette_selected, 0);
    }

    #[test]
    fn enter_dispatches_the_selected_command_and_closes_the_palette() {
        let (mut app, mut receiver) = dispatchable_app(OcppConnectorState::Available);
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the "Vehicle ID" parameter prompt
        app.handle_key_event(key(KeyCode::Char('E')));
        app.handle_key_event(key(KeyCode::Enter)); // submits it, dispatching the command

        assert!(!app.command_palette_open);
        assert!(app.parameter_prompt.is_none());
        assert_eq!(
            receiver.try_recv().unwrap(),
            ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::CableConnected,
                },
            }
        );
        assert!(
            log_messages(&app.logs)
                .iter()
                .any(|l| l.contains("Plug in vehicle sent to CSMS"))
        );
    }

    /// Availability follows the charger's *reported* state, not the dispatch: what changes the list
    /// is the snapshot that comes back once the connector has actually moved.
    #[test]
    fn available_commands_follow_the_snapshot_the_charger_sends_back_after_a_dispatch() {
        let (mut app, mut receiver) = dispatchable_app(OcppConnectorState::Available);
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the parameter prompt
        app.handle_key_event(key(KeyCode::Char('E')));
        app.handle_key_event(key(KeyCode::Enter)); // submits it: plug in vehicle
        assert!(receiver.try_recv().is_ok(), "the event was dispatched");

        // Until the charger says the cable is in, the list is unchanged - the command is still
        // offered, because as far as the charger has reported, the connector is still free.
        assert!(
            app.available_commands()
                .iter()
                .any(|command| command.label() == "Plug in vehicle")
        );

        let (snapshot_sender, snapshot_receiver) = mpsc::unbounded_channel();
        app.charger_snapshot_receiver = Some(snapshot_receiver);
        snapshot_sender
            .send(ChargerSnapshot {
                ocpp: ocpp_state_with(OcppConnectorState::Locked),
                hardware: vec![vec![ConnectorHardwareSnapshot {
                    locked: true,
                    ..Default::default()
                }]],
                campaigns: CampaignProgress::default(),
            })
            .unwrap();
        app.drain_charger_snapshots();

        let labels: Vec<&str> = app.available_commands().iter().map(|c| c.label()).collect();
        assert!(labels.contains(&"Present RFID card"));
        assert!(labels.contains(&"Unplug vehicle"));
        assert!(!labels.contains(&"Plug in vehicle"));
    }

    #[test]
    fn dispatching_a_command_shows_a_confirmation_status_message() {
        let (mut app, _receiver) = dispatchable_app(OcppConnectorState::Available);
        assert_eq!(status(&app), None);

        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the parameter prompt
        app.handle_key_event(key(KeyCode::Char('E')));
        app.handle_key_event(key(KeyCode::Enter)); // submits it

        // `→`, not `✓`: the command was handed to the charger, which is a different claim from
        // "it happened" - the connector moves when the charger says it did.
        assert_eq!(
            status(&app),
            Some((StatusSeverity::Ok, "→ Plug in vehicle".to_string()))
        );
    }

    #[test]
    fn returning_to_the_picker_clears_any_status_message() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.set_status(StatusSeverity::Ok, "✓ Plug in vehicle".to_string());

        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(status(&app), None);
    }

    #[test]
    fn escape_on_the_dashboard_returns_to_the_picker() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::PickCharger);
        assert!(app.charger_state.is_none());
    }

    #[test]
    fn draining_the_log_receiver_appends_every_pending_line() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(vec![]);
        app.log_receiver = Some(receiver);

        sender.send(LogEntry::from("first")).unwrap();
        sender.send(LogEntry::from("second")).unwrap();

        app.drain_log_receiver();
        assert_eq!(log_messages(&app.logs), vec!["first", "second"]);
    }

    #[test]
    fn draining_without_a_receiver_installed_does_nothing() {
        let mut app = App::new(vec![]);
        app.drain_log_receiver();
        assert_eq!(log_messages(&app.logs), Vec::<String>::new());
    }

    /// An app sitting on the dashboard with `count` log entries, for the log-binding tests.
    fn app_with_logs(count: usize) -> App {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.logs = LogBuffer::default();
        for i in 0..count {
            app.logs.push(format!("entry {i}"));
        }
        app
    }

    #[test]
    fn slash_opens_the_log_filter_and_typing_narrows_the_pane_live() {
        let mut app = app_with_logs(0);
        app.logs.push("heartbeat sent");
        app.logs.push("connector faulted");

        app.handle_key_event(key(KeyCode::Char('/')));
        assert!(app.log_filter_open);

        for c in "fault".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        // Narrowed before Enter is ever pressed - a filter that matches nothing is visible as
        // soon as it's typed.
        assert_eq!(log_messages(&app.logs), vec!["connector faulted"]);

        app.handle_key_event(key(KeyCode::Enter));
        assert!(!app.log_filter_open);
        assert_eq!(app.logs.filter(), Some("fault"));
    }

    #[test]
    fn esc_in_the_filter_prompt_clears_the_filter_rather_than_committing_it() {
        let mut app = app_with_logs(0);
        app.logs.push("heartbeat sent");
        app.logs.push("connector faulted");

        app.handle_key_event(key(KeyCode::Char('/')));
        app.handle_key_event(key(KeyCode::Char('f')));
        app.handle_key_event(key(KeyCode::Esc));

        assert!(!app.log_filter_open);
        assert_eq!(app.logs.filter(), None);
        assert_eq!(
            log_messages(&app.logs),
            vec!["heartbeat sent", "connector faulted"]
        );
    }

    #[test]
    fn reopening_the_filter_prompt_seeds_it_with_the_active_filter() {
        let mut app = app_with_logs(0);
        app.logs.push("connector faulted");

        app.handle_key_event(key(KeyCode::Char('/')));
        app.handle_key_event(key(KeyCode::Char('f')));
        app.handle_key_event(key(KeyCode::Enter));
        app.handle_key_event(key(KeyCode::Char('/')));

        assert_eq!(app.log_filter_field.value(), "f");
    }

    #[test]
    fn q_and_question_mark_are_typeable_into_the_log_filter() {
        let mut app = app_with_logs(0);
        app.handle_key_event(key(KeyCode::Char('/')));
        app.handle_key_event(key(KeyCode::Char('q')));
        app.handle_key_event(key(KeyCode::Char('?')));

        assert_eq!(app.log_filter_field.value(), "q?");
        assert!(!app.quit_confirm_open);
        assert!(!app.help_open);
    }

    #[test]
    fn esc_on_the_dashboard_clears_an_active_log_filter_before_it_means_go_back() {
        let mut app = app_with_logs(0);
        app.logs.push("connector faulted");
        app.logs.set_filter("fault");

        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.logs.filter(), None);
        assert_eq!(app.screen, Screen::Dashboard);

        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::PickCharger);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_oldest_and_newest_log_entries() {
        let mut app = app_with_logs(20);

        app.handle_key_event(key(KeyCode::Char('g')));
        assert!(app.logs.is_paused());
        assert_eq!(app.logs.visible_lines(1)[0].message, "entry 0");

        app.handle_key_event(key(KeyCode::Char('G')));
        assert!(!app.logs.is_paused());
        assert_eq!(app.logs.visible_lines(1)[0].message, "entry 19");
    }

    #[test]
    fn l_cycles_the_log_level_threshold() {
        let mut app = app_with_logs(0);
        assert_eq!(app.logs.level_threshold(), LogLevel::Info);

        app.handle_key_event(key(KeyCode::Char('l')));
        assert_eq!(app.logs.level_threshold(), LogLevel::Debug);
    }

    #[test]
    fn ctrl_l_clears_the_log_buffer() {
        let mut app = app_with_logs(3);
        assert_eq!(app.logs.filtered_len(), 3);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));

        assert_eq!(app.logs.filtered_len(), 0);
        assert_eq!(
            status(&app),
            Some((StatusSeverity::Ok, "✓ cleared logs".to_string()))
        );
    }

    #[test]
    fn plain_l_still_cycles_the_level_threshold_and_is_not_shadowed_by_ctrl_l() {
        let mut app = app_with_logs(0);
        assert_eq!(app.logs.level_threshold(), LogLevel::Info);

        app.handle_key_event(key(KeyCode::Char('l')));

        assert_eq!(app.logs.level_threshold(), LogLevel::Debug);
    }

    #[test]
    fn y_copies_the_focused_log_line_and_reports_success() {
        // Touches the real system clipboard (see `copy_focused_log_line`), which is shared,
        // mutable, process-wide state - serialize against `clipboard`'s own tests so they don't
        // race on it.
        let _guard = crate::clipboard::CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let mut app = app_with_logs(0);
        app.logs.push("connector faulted");

        app.handle_key_event(key(KeyCode::Char('y')));

        match status(&app) {
            Some((StatusSeverity::Ok, message)) => assert!(message.contains("copied")),
            other => panic!("expected a success status message, got {other:?}"),
        }
    }

    #[test]
    fn y_with_no_log_lines_reports_an_error_instead_of_copying_nothing() {
        let mut app = app_with_logs(0);

        app.handle_key_event(key(KeyCode::Char('y')));

        assert_eq!(
            status(&app),
            Some((StatusSeverity::Error, "✗ no log line to copy".to_string()))
        );
    }

    #[test]
    fn ctrl_k_opens_the_command_palette_and_c_still_works_as_an_alias() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        app.handle_key_event(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(app.command_palette_open);

        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Char('c')));
        assert!(app.command_palette_open);
    }

    #[test]
    fn a_bare_k_does_not_open_the_palette() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Char('k')));
        assert!(!app.command_palette_open);
    }

    #[test]
    fn the_palette_matches_commands_as_a_fuzzy_subsequence() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        for c in "pv".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }

        // "pv" is not a substring of any label - the old matcher found nothing here.
        let labels: Vec<&str> = app.palette_commands().iter().map(|c| c.label()).collect();
        assert_eq!(labels.first(), Some(&"Plug in vehicle"));
    }

    #[test]
    fn tab_retargets_the_palette_to_the_next_connector() {
        let mut app = App::new(vec![charger_with_evses(
            "CP-2C",
            vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
        )]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        assert_eq!(
            app.focused_connector_label().as_deref(),
            Some("EVSE 1 / C1")
        );

        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(
            app.focused_connector_label().as_deref(),
            Some("EVSE 1 / C2")
        );
        // Still open: retargeting happens in place, without a round trip through the dashboard.
        assert!(app.command_palette_open);
        assert_eq!(app.command_palette_selected, 0);

        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(
            app.focused_connector_label().as_deref(),
            Some("EVSE 1 / C1")
        );
    }

    #[test]
    fn a_blank_parameter_is_rejected_inline_and_leaves_the_prompt_open() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter));

        app.handle_key_event(key(KeyCode::Enter)); // submit blank

        assert!(app.parameter_prompt.is_some());
        assert_eq!(app.parameter_error, Some("cannot be blank"));
        assert_eq!(status(&app), None);

        // Typing clears the complaint about the value that's no longer there.
        app.handle_key_event(key(KeyCode::Char('E')));
        assert_eq!(app.parameter_error, None);
    }

    #[test]
    fn whitespace_only_counts_as_blank() {
        assert_eq!(validate_parameter("   "), Some("cannot be blank"));
        assert_eq!(validate_parameter("EV-1"), None);
    }

    #[test]
    fn a_parameter_prompt_is_prefilled_with_the_last_accepted_value() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter));
        for c in "MY-EV".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(key(KeyCode::Enter));

        // Unplug, then plug in again: the vehicle id is remembered.
        app.apply_command(Command::UnplugVehicle, "");
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.parameter_field.value(), "MY-EV");
        // Prefilled, not merely suggested: Enter alone submits it.
        assert_eq!(app.parameter_field.cursor(), "MY-EV".chars().count());
    }

    #[test]
    fn a_rejected_parameter_is_not_remembered() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter));
        app.handle_key_event(key(KeyCode::Enter)); // blank, rejected

        assert!(app.parameter_history.is_empty());
    }

    #[test]
    fn a_status_message_expires_after_its_ttl() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.set_status(StatusSeverity::Ok, "✓ Plug in vehicle".to_string());
        let shown_at = app.status_message.as_ref().unwrap().shown_at;

        // Just short of the TTL: still there, so the message is actually readable.
        app.tick_metrics_with(
            Duration::ZERO,
            shown_at + STATUS_MESSAGE_TTL - Duration::from_millis(1),
        );
        assert!(app.status_message.is_some());

        app.tick_metrics_with(Duration::ZERO, shown_at + STATUS_MESSAGE_TTL);
        assert_eq!(status(&app), None);
    }

    #[test]
    fn selecting_a_2_1_charger_goes_to_connection_setup_not_the_dashboard() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        assert_eq!(app.screen, Screen::ConnectionSetup);
    }

    #[test]
    fn selecting_a_1_6j_or_2_0_1_charger_still_goes_straight_to_the_dashboard() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn connection_setup_defaults_to_an_empty_url_and_the_charger_id_as_identity() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        assert_eq!(app.connection_csms_url.value(), "");
        assert_eq!(app.connection_ocpp_identity.value(), "CP-2.1");
        assert_eq!(app.connection_password.value(), "");
        assert_eq!(app.connection_focused_field, 0);
    }

    #[test]
    fn connection_setup_prefills_from_a_remembered_profile() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.connection_store.remember(
            "CP-2.1",
            ConnectionProfile {
                csms_url: "wss://csms.example.com".into(),
                ocpp_identity: "remembered-id".into(),
                security: SecurityProfile::Basic {
                    password: "secret".into(),
                },
            },
        );

        app.confirm_charger_selection();

        assert_eq!(app.connection_csms_url.value(), "wss://csms.example.com");
        assert_eq!(app.connection_ocpp_identity.value(), "remembered-id");
        assert_eq!(app.connection_password.value(), "secret");
    }

    #[test]
    fn tab_and_shifttab_move_focus_between_fields_and_clamp() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.connection_focused_field, 1);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.connection_focused_field, 2);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.connection_focused_field, 2);

        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(app.connection_focused_field, 1);
    }

    #[test]
    fn typing_edits_the_focused_field() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        for c in "ws://host".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        assert_eq!(app.connection_csms_url.value(), "ws://host");

        app.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(app.connection_csms_url.value(), "ws://hos");
    }

    #[test]
    fn q_and_question_mark_are_typed_into_the_field_instead_of_opening_global_modals() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Char('q')));
        app.handle_key_event(key(KeyCode::Char('?')));

        assert_eq!(app.connection_csms_url.value(), "q?");
        assert!(!app.quit_confirm_open);
        assert!(!app.help_open);
    }

    #[test]
    fn esc_on_connection_setup_returns_to_the_picker() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::PickCharger);
    }

    /// The other entry point into [`App::spawn_local_charger`] (see
    /// `a_locally_selected_charger_reports_offline_rather_than_booting_forever` for the
    /// 1.6J/2.0.1 one): a 2.1 charger that reaches the connection setup screen but is confirmed
    /// with a blank CSMS URL. Same real background thread, same bounded poll for its first
    /// snapshot - see that test's doc comment for why this isn't flaky in practice.
    #[test]
    fn confirming_connection_setup_with_a_blank_url_runs_locally_and_reports_offline() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        assert_eq!(app.connection_csms_url.value(), "");

        app.confirm_connection_setup();

        assert_eq!(app.screen, Screen::Dashboard);
        assert_eq!(
            app.charger_state.as_ref().unwrap().mode,
            SimulationMode::Local
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            app.drain_charger_snapshots();
            if app.charger_state.as_ref().unwrap().connection_status == ConnectionStatus::Offline {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "never received a snapshot from the local charger's background thread"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn confirming_with_a_url_remembers_the_profile_and_starts_a_connect_attempt() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        for c in "ws://localhost:9999/dev".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        // identity already defaults to the charger id (see
        // `connection_setup_defaults_to_an_empty_url_and_the_charger_id_as_identity`)

        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.screen, Screen::Dashboard);
        assert!(app.connect_result_receiver.is_some());
        assert!(app.charger_snapshot_receiver.is_some());
        assert!(app.ocpp_event_sender.is_some());
        let remembered = app.connection_store.get("CP-2.1").unwrap();
        assert_eq!(remembered.csms_url, "ws://localhost:9999/dev");
        assert_eq!(remembered.ocpp_identity, "CP-2.1");
    }

    #[test]
    fn confirming_with_a_url_puts_the_charger_in_live_csms_mode_with_that_url() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        for c in "ws://localhost:9999/dev".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(
            app.charger_state.unwrap().mode,
            SimulationMode::LiveCsms {
                url: "ws://localhost:9999/dev".to_string()
            }
        );
    }

    #[test]
    fn confirming_with_a_blank_url_does_not_start_a_connect_attempt() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.screen, Screen::Dashboard);
        assert!(app.connect_result_receiver.is_none());
    }

    #[test]
    fn confirming_with_a_blank_url_leaves_the_charger_in_local_mode() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.charger_state.unwrap().mode, SimulationMode::Local);
    }

    #[test]
    fn validate_csms_url_accepts_blank_ws_and_wss() {
        assert_eq!(validate_csms_url(""), None);
        assert_eq!(validate_csms_url("   "), None);
        assert_eq!(validate_csms_url("ws://host"), None);
        assert_eq!(validate_csms_url("wss://host"), None);
    }

    #[test]
    fn validate_csms_url_rejects_any_other_scheme() {
        assert_eq!(
            validate_csms_url("http://host"),
            Some("must start with ws:// or wss://")
        );
        assert_eq!(
            validate_csms_url("host.example.com"),
            Some("must start with ws:// or wss://")
        );
    }

    #[test]
    fn confirming_with_an_invalid_url_scheme_shows_an_inline_error_and_stays_put() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        for c in "http://host".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.screen, Screen::ConnectionSetup);
        assert_eq!(
            app.connection_url_error,
            Some("must start with ws:// or wss://")
        );
        assert!(app.connect_result_receiver.is_none());
    }

    #[test]
    fn editing_the_url_field_clears_its_error() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        for c in "http://host".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(key(KeyCode::Enter));
        assert!(app.connection_url_error.is_some());

        app.handle_key_event(key(KeyCode::Char('s')));
        assert_eq!(app.connection_url_error, None);
    }

    #[test]
    fn password_reveal_starts_off_and_ctrl_r_toggles_it() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        assert!(!app.connection_password_revealed);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(app.connection_password_revealed);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(!app.connection_password_revealed);
    }

    #[test]
    fn plain_r_is_still_typed_into_the_focused_field() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('r')));

        assert_eq!(app.connection_csms_url.value(), "r");
        assert!(!app.connection_password_revealed);
    }

    #[test]
    fn page_down_cycles_forward_through_recent_url_suggestions_on_the_url_field() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.connection_store.remember(
            "CP-OTHER-A",
            ConnectionProfile {
                csms_url: "wss://a.example.com".into(),
                ocpp_identity: "x".into(),
                security: SecurityProfile::Basic {
                    password: String::new(),
                },
            },
        );
        app.connection_store.remember(
            "CP-OTHER-B",
            ConnectionProfile {
                csms_url: "wss://b.example.com".into(),
                ocpp_identity: "x".into(),
                security: SecurityProfile::Basic {
                    password: String::new(),
                },
            },
        );
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::PageDown));
        assert_eq!(app.connection_csms_url.value(), "wss://a.example.com");

        app.handle_key_event(key(KeyCode::PageDown));
        assert_eq!(app.connection_csms_url.value(), "wss://b.example.com");

        // Wraps back around to the first suggestion.
        app.handle_key_event(key(KeyCode::PageDown));
        assert_eq!(app.connection_csms_url.value(), "wss://a.example.com");
    }

    #[test]
    fn page_up_cycles_backward_through_recent_url_suggestions() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.connection_store.remember(
            "CP-OTHER-A",
            ConnectionProfile {
                csms_url: "wss://a.example.com".into(),
                ocpp_identity: "x".into(),
                security: SecurityProfile::Basic {
                    password: String::new(),
                },
            },
        );
        app.connection_store.remember(
            "CP-OTHER-B",
            ConnectionProfile {
                csms_url: "wss://b.example.com".into(),
                ocpp_identity: "x".into(),
                security: SecurityProfile::Basic {
                    password: String::new(),
                },
            },
        );
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::PageUp));
        assert_eq!(app.connection_csms_url.value(), "wss://b.example.com");
    }

    #[test]
    fn typing_narrows_recent_url_suggestions_by_prefix() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.connection_store.remember(
            "CP-OTHER-A",
            ConnectionProfile {
                csms_url: "wss://a.example.com".into(),
                ocpp_identity: "x".into(),
                security: SecurityProfile::Basic {
                    password: String::new(),
                },
            },
        );
        app.connection_store.remember(
            "CP-OTHER-B",
            ConnectionProfile {
                csms_url: "wss://b.example.com".into(),
                ocpp_identity: "x".into(),
                security: SecurityProfile::Basic {
                    password: String::new(),
                },
            },
        );
        app.confirm_charger_selection();

        for c in "wss://a".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }

        assert_eq!(
            app.connection_url_suggestions(),
            vec!["wss://a.example.com".to_string()]
        );
    }

    #[test]
    fn page_down_on_a_field_other_than_the_url_does_nothing() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.connection_store.remember(
            "CP-OTHER-A",
            ConnectionProfile {
                csms_url: "wss://a.example.com".into(),
                ocpp_identity: "x".into(),
                security: SecurityProfile::Basic {
                    password: String::new(),
                },
            },
        );
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Tab)); // focus OCPP identity

        app.handle_key_event(key(KeyCode::PageDown));

        assert_eq!(app.connection_csms_url.value(), "");
    }

    #[test]
    fn resolve_simulation_mode_is_local_for_a_blank_or_whitespace_only_url() {
        assert_eq!(resolve_simulation_mode(""), SimulationMode::Local);
        assert_eq!(resolve_simulation_mode("   "), SimulationMode::Local);
    }

    #[test]
    fn resolve_simulation_mode_is_live_csms_with_the_url_for_a_non_blank_url() {
        assert_eq!(
            resolve_simulation_mode("wss://csms.example.com"),
            SimulationMode::LiveCsms {
                url: "wss://csms.example.com".to_string()
            }
        );
    }

    #[test]
    fn a_live_csms_charger_does_not_self_promote_to_connected_on_ticks() {
        // With a real CSMS connection in the picture, `connection_status` must come from the
        // OCPP bridge alone (see `SimulationMode`'s doc comment) - `tick`'s simulated boot
        // timer must never race it and flip the dashboard to "connected" on its own.
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().mode = SimulationMode::LiveCsms {
            url: "wss://csms.example.com".to_string(),
        };

        app.tick_metrics_with(Duration::from_secs(60), Instant::now());

        assert_eq!(
            app.charger_state.unwrap().connection_status,
            ConnectionStatus::Booting
        );
    }

    #[test]
    fn returning_to_the_picker_leaves_no_stale_live_csms_mode_for_the_next_charger() {
        let mut app = App::new(vec![charger_v21("CP-2.1"), charger("CP001")]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().mode = SimulationMode::LiveCsms {
            url: "wss://csms.example.com".to_string(),
        };

        app.handle_key_event(key(KeyCode::Esc)); // back to the picker
        app.handle_key_event(key(KeyCode::Down)); // select CP001 (1.6J, straight to dashboard)
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.charger_state.unwrap().mode, SimulationMode::Local);
    }

    #[test]
    fn poll_connect_result_reports_success() {
        let (sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![]);
        app.connect_result_receiver = Some(receiver);
        sender.send(Ok(())).unwrap();

        app.poll_connect_result();

        assert_eq!(
            status(&app),
            Some((StatusSeverity::Ok, "✓ connected to CSMS".to_string()))
        );
        assert!(app.connect_result_receiver.is_none());
    }

    #[test]
    fn poll_connect_result_reports_failure() {
        let (sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![]);
        app.connect_result_receiver = Some(receiver);
        sender.send(Err("boom".to_string())).unwrap();

        app.poll_connect_result();

        assert_eq!(
            status(&app),
            Some((
                StatusSeverity::Error,
                "✗ CSMS connection failed: boom".to_string()
            ))
        );
        assert!(app.connect_result_receiver.is_none());
    }

    /// The point of `connection_failure`: a toast expires after `STATUS_MESSAGE_TTL`, and a charger
    /// whose dial failed will never report anything again, so the four-second version of this message
    /// was the only version there was.
    #[test]
    fn a_failed_connection_is_recorded_where_it_cannot_expire() {
        let (sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.connect_result_receiver = Some(receiver);
        sender
            .send(Err("dns error: no such host".to_string()))
            .unwrap();

        app.poll_connect_result();

        assert_eq!(
            app.connection_failure.as_deref(),
            Some("dns error: no such host")
        );

        // The toast goes, as every toast should; the failure stays.
        app.expire_status_message(Instant::now() + STATUS_MESSAGE_TTL);
        assert_eq!(status(&app), None);
        assert_eq!(
            app.connection_failure.as_deref(),
            Some("dns error: no such host")
        );
    }

    /// It also goes in the trace, at `Error` level - `Info` is what the level threshold hides first,
    /// and this is the line that explains everything else on screen.
    #[test]
    fn a_failed_connection_is_logged_as_an_error() {
        let (sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.connect_result_receiver = Some(receiver);
        sender.send(Err("connection refused".to_string())).unwrap();

        app.poll_connect_result();

        let entry = app
            .logs
            .visible_lines(10)
            .into_iter()
            .find(|entry| entry.message.contains("connection refused"))
            .expect("the failure should be in the log");
        assert_eq!(entry.level, LogLevel::Error);
    }

    #[test]
    fn a_successful_connection_clears_a_previous_failure() {
        let (sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.connection_failure = Some("connection refused".to_string());
        app.connect_result_receiver = Some(receiver);
        sender.send(Ok(())).unwrap();

        app.poll_connect_result();

        assert!(app.connection_failure.is_none());
    }

    /// `r` is the way out of the dead end a failed dial leaves: back to the setup screen, every field
    /// prefilled from the profile that failed, so a typo'd URL is an edit rather than a retype.
    #[test]
    fn r_reopens_connection_setup_prefilled_after_a_failure() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.connection_store.remember(
            "CP-2.1".to_string(),
            ConnectionProfile {
                csms_url: "ws://typo.example/CP-2.1".into(),
                ocpp_identity: "CP-2.1".into(),
                security: SecurityProfile::Basic {
                    password: "hunter2".into(),
                },
            },
        );
        app.screen = Screen::Dashboard;
        app.connection_failure = Some("connection refused".to_string());

        app.handle_key_event(key(KeyCode::Char('r')));

        assert_eq!(app.screen, Screen::ConnectionSetup);
        assert_eq!(app.connection_csms_url.value(), "ws://typo.example/CP-2.1");
        assert_eq!(app.connection_password.value(), "hunter2");
        assert!(
            app.connection_failure.is_none(),
            "the failure has been acted on"
        );
    }

    /// Not a general reconnect: a connected charger must not be torn down by a stray keypress, and a
    /// 1.6J/2.0.1 charger never had a CSMS to retry against in the first place.
    #[test]
    fn r_does_nothing_when_there_is_no_failure_to_retry() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Char('r')));

        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn returning_to_the_picker_forgets_the_failure() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.screen = Screen::Dashboard;
        app.connection_failure = Some("connection refused".to_string());

        app.handle_key_event(key(KeyCode::Esc));

        assert!(app.connection_failure.is_none());
    }

    #[test]
    fn poll_connect_result_leaves_status_untouched_while_still_pending() {
        let (_sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![]);
        app.connect_result_receiver = Some(receiver);

        app.poll_connect_result();

        assert_eq!(status(&app), None);
        assert!(app.connect_result_receiver.is_some());
    }

    fn ocpp_state_with(connector: OcppConnectorState) -> ChargePointState {
        let mut state = ChargePointState::new([1]);
        state.registration = Some(RegistrationStatus::Accepted);
        state.evses[0].connectors[0] = connector;
        state
    }

    #[test]
    fn tick_metrics_does_nothing_without_a_selected_charger() {
        let mut app = App::new(vec![]);
        // Just proving this doesn't panic with no charger selected.
        app.tick_metrics_with(Duration::from_secs(1), Instant::now());
        assert!(app.charger_state.is_none());
    }

    /// H3b: the meter itself lives in `core`'s hardware layer now, driven by
    /// `RunningCharger::tick` on the charger's background thread - `tick_metrics_with` only
    /// forwards the elapsed duration there. Injects `ocpp_tick_sender` directly (the same style
    /// `apply_command`'s connected-mode tests inject `ocpp_event_sender`/`live_ocpp_state`)
    /// rather than exercising the real spawned thread, so this stays a fast, deterministic test
    /// of the wiring - the physics themselves are `core`'s to test.
    #[test]
    fn tick_metrics_forwards_the_elapsed_duration_to_the_running_chargers_tick_channel() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_tick_sender = Some(sender);

        app.tick_metrics_with(Duration::from_secs(3600), Instant::now());

        assert_eq!(receiver.try_recv().unwrap(), Duration::from_secs(3600));
    }

    /// Every tick forwards, with no throttling - unlike the old `MeterValueSampled` push this
    /// replaced, there is no reason to hold anything back: `RunningCharger::tick` is meant to be
    /// called on exactly the cadence the app already ticks at (H3b).
    #[test]
    fn tick_metrics_forwards_every_tick_with_no_throttling() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_tick_sender = Some(sender);

        let now = Instant::now();
        app.tick_metrics_with(Duration::from_millis(100), now);
        app.tick_metrics_with(Duration::from_millis(100), now);

        assert_eq!(receiver.try_recv().unwrap(), Duration::from_millis(100));
        assert_eq!(receiver.try_recv().unwrap(), Duration::from_millis(100));
    }

    #[test]
    fn tick_metrics_does_nothing_without_a_running_chargers_tick_channel() {
        let mut app = App::new(vec![]);

        // No panic and nothing to send through, since there's no `ocpp_tick_sender` at all.
        app.tick_metrics_with(Duration::from_secs(3600), Instant::now());
        assert!(app.ocpp_tick_sender.is_none());
    }

    #[test]
    fn drain_charger_snapshots_applies_both_halves_and_remembers_the_latest_protocol_state() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        // H3b: `apply_ocpp_state` only reads `registration` for a `LiveCsms` charger - a `Local`
        // one (what `confirm_charger_selection` alone leaves this charger as, since it hasn't
        // gone through `confirm_connection_setup` yet) always reads `Offline` regardless. This
        // test is specifically about the connected path, so give it a CSMS to be connected to.
        app.charger_state.as_mut().unwrap().mode = SimulationMode::LiveCsms {
            url: "ws://csms.example/CP-2.1".into(),
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        app.charger_snapshot_receiver = Some(receiver);

        sender
            .send(ChargerSnapshot {
                ocpp: ocpp_state_with(OcppConnectorState::Locked),
                // H7/H14: none of these five facts exist anywhere in `ocpp`, which is the whole
                // reason a snapshot carries a second half at all.
                hardware: vec![vec![ConnectorHardwareSnapshot {
                    locked: true,
                    contactor_closed: true,
                    current_limit_ma: Some(16_000),
                    discharging: true,
                    exported_energy_wh: 1_250,
                }]],
                campaigns: CampaignProgress {
                    firmware_install: Some(FirmwareInstallStage::Installing),
                    ..Default::default()
                },
            })
            .unwrap();
        app.drain_charger_snapshots();

        assert_eq!(
            app.charger_state.as_ref().unwrap().connection_status,
            ConnectionStatus::Connected
        );
        assert_eq!(
            app.live_ocpp_state.as_ref().unwrap().evses[0].connectors[0],
            OcppConnectorState::Locked
        );

        let connector = &app.charger_state.as_ref().unwrap().evses[0].connectors[0];
        assert!(connector.locked);
        assert!(connector.contactor_closed);
        assert_eq!(connector.current_limit_ma, Some(16_000));
        assert!(connector.discharging);
        assert_eq!(connector.exported_energy_wh, 1_250);

        // The charger-wide half: a firmware campaign has no connector to live on.
        assert_eq!(
            app.campaigns.firmware_install,
            Some(FirmwareInstallStage::Installing)
        );
    }

    // --- the hardware bundle (H10/H12) ----------------------------------------------------

    /// A transfer `fraction` of the way through the simulated firmware image, shaped exactly as
    /// `FakeFileTransfer` would report it.
    fn in_flight_transfer(fraction: f64) -> InFlightTransfer {
        let total_bytes = SIMULATED_FIRMWARE_DOWNLOAD.total_bytes;
        InFlightTransfer {
            elapsed: SIMULATED_FIRMWARE_DOWNLOAD.duration.mul_f64(fraction),
            duration: SIMULATED_FIRMWARE_DOWNLOAD.duration,
            transferred_bytes: (total_bytes as f64 * fraction) as u64,
            total_bytes,
        }
    }

    fn hardware_for(declare: impl FnOnce(&mut CapabilitiesConfig)) -> ChargerHardware {
        let mut config = charger("CP-HW").config;
        declare(&mut config.capabilities);
        charger_hardware(&config)
    }

    /// Storage and a display are handed over regardless (H5b/H6b's own gating decides whether
    /// anything is registered); every other piece needs a declaration.
    #[test]
    fn a_charger_declaring_nothing_gets_no_firmware_transfer_or_certificate_hardware() {
        let hardware = hardware_for(|_| {});

        assert!(hardware.storage.is_some());
        assert!(hardware.display.is_some());
        assert!(hardware.firmware_installer.is_none());
        assert!(hardware.firmware_verifier.is_none());
        assert!(hardware.file_transfer.is_none());
        assert!(hardware.certificate_store.is_none());
    }

    /// The verifier comes with the installer, not separately: upstream refuses signed updates
    /// without one, so an installer alone could only ever demonstrate half a campaign.
    #[test]
    fn declaring_firmware_management_supplies_an_installer_a_verifier_and_a_file_transfer() {
        let hardware = hardware_for(|capabilities| capabilities.firmware_management = true);

        assert!(hardware.firmware_installer.is_some());
        assert!(hardware.firmware_verifier.is_some());
        assert!(
            hardware.file_transfer.is_some(),
            "a firmware campaign has to fetch the image before installing it"
        );
        assert!(hardware.certificate_store.is_none());
    }

    /// Diagnostics needs the same file transfer for its log upload, but no installer - a charger
    /// that can upload logs isn't thereby able to install firmware.
    #[test]
    fn declaring_diagnostics_supplies_only_the_file_transfer() {
        let hardware = hardware_for(|capabilities| capabilities.diagnostics = true);

        assert!(hardware.file_transfer.is_some());
        assert!(hardware.firmware_installer.is_none());
        assert!(hardware.firmware_verifier.is_none());
    }

    #[test]
    fn declaring_certificate_management_supplies_a_certificate_store() {
        let hardware = hardware_for(|capabilities| capabilities.certificate_management = true);

        assert!(hardware.certificate_store.is_some());
        assert!(hardware.file_transfer.is_none());
    }

    /// Nothing registers a `KeyStore` (see `charger_hardware`'s doc comment), so supplying one would
    /// imply a capability the charger doesn't have - even for a charger that declares `key_storage`.
    #[test]
    fn no_key_store_is_supplied_even_when_key_storage_is_declared() {
        assert!(
            hardware_for(|capabilities| capabilities.key_storage = true)
                .key_store
                .is_none()
        );
    }

    /// The link between the bundle and the strip: a charger with firmware hardware reports a stage
    /// (idle, since nothing has been asked of it), one without reports nothing at all - the
    /// distinction the strip relies on to tell "cannot install firmware" from "has nothing to
    /// install".
    #[test]
    fn campaign_handles_report_a_stage_only_for_hardware_the_charger_actually_has() {
        let with_firmware = CampaignHandles::of(&hardware_for(|capabilities| {
            capabilities.firmware_management = true
        }))
        .progress();
        assert_eq!(
            with_firmware.firmware_install,
            Some(FirmwareInstallStage::Idle)
        );
        assert_eq!(with_firmware.firmware_download, None, "nothing started yet");
        assert!(!with_firmware.is_active());

        let without = CampaignHandles::of(&hardware_for(|_| {})).progress();
        assert_eq!(without, CampaignProgress::default());
    }

    #[test]
    fn campaign_progress_is_active_only_while_something_is_actually_happening() {
        assert!(!CampaignProgress::default().is_active());
        assert!(
            !CampaignProgress {
                firmware_install: Some(FirmwareInstallStage::Idle),
                ..Default::default()
            }
            .is_active(),
            "an installer with nothing to do is not activity"
        );
        assert!(
            CampaignProgress {
                firmware_install: Some(FirmwareInstallStage::Installing),
                ..Default::default()
            }
            .is_active()
        );
        assert!(
            CampaignProgress {
                firmware_install: Some(FirmwareInstallStage::Failed),
                ..Default::default()
            }
            .is_active(),
            "a failed install is exactly what a user needs to see"
        );
        assert!(
            CampaignProgress {
                log_upload: Some(in_flight_transfer(0.5)),
                ..Default::default()
            }
            .is_active()
        );
    }

    #[test]
    fn apply_command_sends_the_matching_event_when_connected_to_a_real_csms() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);
        app.live_ocpp_state = Some(ocpp_state_with(OcppConnectorState::Available));

        app.apply_command(Command::PlugInVehicle, "");

        let sent = receiver.try_recv().unwrap();
        assert_eq!(
            sent,
            ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::CableConnected,
                },
            }
        );
    }

    #[test]
    fn apply_command_reports_not_ready_when_no_connector_is_eligible_yet() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);
        app.live_ocpp_state = Some(ocpp_state_with(OcppConnectorState::Charging));

        app.apply_command(Command::PlugInVehicle, "");

        assert!(receiver.try_recv().is_err());
        assert_eq!(
            status(&app),
            Some((
                StatusSeverity::Error,
                "✗ Plug in vehicle not ready yet".to_string()
            ))
        );
    }

    #[test]
    fn returning_to_the_picker_tears_down_the_live_connection_channels() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        let (_sender, receiver) = mpsc::unbounded_channel::<ChargerSnapshot>();
        let (event_sender, _event_receiver) = mpsc::unbounded_channel();
        app.charger_snapshot_receiver = Some(receiver);
        app.ocpp_event_sender = Some(event_sender);
        app.live_ocpp_state = Some(ocpp_state_with(OcppConnectorState::Available));

        app.handle_key_event(key(KeyCode::Esc));

        assert!(app.charger_snapshot_receiver.is_none());
        assert!(app.ocpp_event_sender.is_none());
        assert!(app.live_ocpp_state.is_none());
    }

    // --- V2G discharge control (H14b) -----------------------------------------------------

    /// A charger declaring `supports_bidirectional_power`, already on the dashboard with a vehicle
    /// plugged into its only connector and the discharge control channel wired to `receiver`'s
    /// counterpart (rather than a real background thread - the same injection style the
    /// `apply_command` connected-mode tests use).
    fn v2g_app() -> (App, UnboundedReceiver<HardwareControl>) {
        let mut entry = charger("CP-V2G");
        entry.config.capabilities.supports_bidirectional_power = true;
        let mut app = App::new(vec![entry]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Charging;

        let (sender, receiver) = mpsc::unbounded_channel();
        app.hardware_control_sender = Some(sender);
        (app, receiver)
    }

    #[test]
    fn d_asks_the_hardware_to_export_on_the_focused_connector() {
        let (mut app, mut receiver) = v2g_app();

        app.handle_key_event(key(KeyCode::Char('d')));

        assert_eq!(
            receiver.try_recv().unwrap(),
            HardwareControl::SetDischarging {
                evse: 0,
                connector: 0,
                discharging: true,
            }
        );
        // The message names the connector it resolved to, so a misdirected toggle is visible.
        assert_eq!(
            status(&app),
            Some((
                StatusSeverity::Ok,
                "→ EVSE 1 connector 1: exporting (V2G)".to_string()
            ))
        );
    }

    /// The toggle reads the direction off `ChargerState` - i.e. off the last snapshot the hardware
    /// sent - so pressing `d` on a connector already exporting asks for import, not export again.
    #[test]
    fn d_asks_for_import_again_when_the_connector_is_already_exporting() {
        let (mut app, mut receiver) = v2g_app();
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].discharging = true;

        app.handle_key_event(key(KeyCode::Char('d')));

        assert_eq!(
            receiver.try_recv().unwrap(),
            HardwareControl::SetDischarging {
                evse: 0,
                connector: 0,
                discharging: false,
            }
        );
    }

    #[test]
    fn d_targets_the_focused_connector_not_the_first_one() {
        let mut entry = charger_with_evses(
            "CP-V2G",
            vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
        );
        entry.config.capabilities.supports_bidirectional_power = true;
        let mut app = App::new(vec![entry]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().evses[0].connectors[1].status =
            ConnectorStatus::Charging;
        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.hardware_control_sender = Some(sender);

        app.handle_key_event(key(KeyCode::Char('d')));

        assert_eq!(
            receiver.try_recv().unwrap(),
            HardwareControl::SetDischarging {
                evse: 0,
                connector: 1,
                discharging: true,
            }
        );
    }

    /// The hardware would obey either way; declaring is what the CSMS under test can see, so a
    /// charger that told it "no bidirectional power" must not then export - see
    /// `toggle_discharging`'s doc comment.
    #[test]
    fn d_is_refused_on_a_charger_that_does_not_declare_bidirectional_power() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Charging;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.hardware_control_sender = Some(sender);

        app.handle_key_event(key(KeyCode::Char('d')));

        assert!(receiver.try_recv().is_err());
        assert_eq!(
            status(&app),
            Some((
                StatusSeverity::Error,
                "✗ CP001 does not declare bidirectional power".to_string()
            ))
        );
    }

    #[test]
    fn d_is_refused_with_nothing_plugged_in() {
        let (mut app, mut receiver) = v2g_app();
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Available;

        app.handle_key_event(key(KeyCode::Char('d')));

        assert!(receiver.try_recv().is_err());
        assert_eq!(
            status(&app),
            Some((
                StatusSeverity::Error,
                "✗ V2G needs a vehicle plugged in".to_string()
            ))
        );
    }

    /// The one test that drives `d` through the *real* background thread rather than a hand-injected
    /// channel, so `drive_running_charger`'s control loop, `RunningCharger::set_discharging`, the
    /// snapshot it publishes and `apply_hardware_snapshot` are all exercised end to end - the seam
    /// every other test in this section stubs out. Same shape (and same bounded polling) as
    /// `a_locally_selected_charger_reports_offline_rather_than_booting_forever`, which is the other
    /// place this suite lets a real charger thread run.
    ///
    /// Declares `supports_bidirectional_power` only, so nothing is persisted: the bundle's
    /// `FileStorage` is never read from or written to without `has_persistent_storage` (see
    /// `charger_hardware`), and this test touches no disk.
    #[test]
    fn d_reaches_the_real_hardware_and_comes_back_in_a_snapshot() {
        let mut entry = charger("CP-V2G-LIVE");
        entry.config.capabilities.supports_bidirectional_power = true;
        let mut app = App::new(vec![entry]);
        app.confirm_charger_selection();

        // A vehicle has to be plugged in for the toggle to be offered. Set locally and pressed
        // immediately: the next snapshot drained will overwrite `status` from the real (still
        // `Available`) protocol state, but direction is a property of the connector's hardware and
        // survives that - which is itself part of what this asserts.
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Charging;
        app.handle_key_event(key(KeyCode::Char('d')));

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.drain_charger_snapshots();
            if app.charger_state.as_ref().unwrap().evses[0].connectors[0].discharging {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the discharge never came back from the charger's own hardware"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // --- local firmware and diagnostics campaigns (H10) -----------------------------------

    /// A charger declaring firmware management and diagnostics, on the dashboard, with the control
    /// channel stubbed so what the palette dispatches can be observed.
    fn campaign_app() -> (App, UnboundedReceiver<HardwareControl>) {
        let mut entry = charger("CP-FW");
        entry.config.capabilities.firmware_management = true;
        entry.config.capabilities.diagnostics = true;
        let mut app = App::new(vec![entry]);
        app.confirm_charger_selection();
        // What `CampaignHandles::progress` reports for this charger's real bundle: hardware present,
        // nothing in flight.
        app.campaigns = CampaignProgress {
            firmware_install: Some(FirmwareInstallStage::Idle),
            ..Default::default()
        };

        let (sender, receiver) = mpsc::unbounded_channel();
        app.hardware_control_sender = Some(sender);
        (app, receiver)
    }

    fn dispatch_from_palette(app: &mut App, filter: &str) {
        app.handle_key_event(key(KeyCode::Char('c')));
        for c in filter.chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(key(KeyCode::Enter));
    }

    /// The gap this closes: in local mode `register_optional_hardware` never registers
    /// `firmware_updates`, so before this the installer existed, was ticked, and could never be asked
    /// to do anything.
    #[test]
    fn the_palette_can_start_a_firmware_update_with_no_csms_in_the_picture() {
        let (mut app, mut receiver) = campaign_app();

        dispatch_from_palette(&mut app, "install firmware");

        assert_eq!(
            receiver.try_recv().unwrap(),
            HardwareControl::InstallFirmware
        );
        assert_eq!(
            status(&app),
            Some((
                StatusSeverity::Ok,
                "→ firmware update started (no CSMS)".to_string()
            ))
        );
        assert!(
            log_messages(&app.logs)
                .iter()
                .any(|line| line.contains("firmware update started"))
        );
    }

    #[test]
    fn the_palette_can_start_a_diagnostics_upload_and_arm_each_failure_independently() {
        let (mut app, mut receiver) = campaign_app();

        dispatch_from_palette(&mut app, "upload diagnostics");
        assert_eq!(
            receiver.try_recv().unwrap(),
            HardwareControl::UploadDiagnostics
        );

        dispatch_from_palette(&mut app, "fail firmware downloads");
        assert_eq!(
            receiver.try_recv().unwrap(),
            HardwareControl::FailFirmwareDownload
        );

        dispatch_from_palette(&mut app, "fail diagnostics uploads");
        assert_eq!(
            receiver.try_recv().unwrap(),
            HardwareControl::FailDiagnosticsUpload
        );
    }

    /// The palette lists a hardware action only where the charger declares the hardware behind it -
    /// the same treatment an ineligible command gets, and the reason there is no greyed-out row.
    #[test]
    fn a_charger_declaring_nothing_gets_no_hardware_rows_in_the_palette() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        let labels: Vec<&str> = app
            .palette_entries()
            .iter()
            .map(|entry| entry.label())
            .collect();

        assert!(labels.contains(&"Plug in vehicle"), "{labels:?}");
        for action in HardwareAction::ALL {
            assert!(!labels.contains(&action.label()), "{labels:?}");
        }
    }

    #[test]
    fn a_declaring_charger_gets_its_hardware_rows_after_the_protocol_commands() {
        let (app, _receiver) = campaign_app();

        let labels: Vec<&str> = app
            .palette_entries()
            .iter()
            .map(|entry| entry.label())
            .collect();

        assert!(labels.contains(&"Install firmware locally"), "{labels:?}");
        assert!(labels.contains(&"Upload diagnostics locally"), "{labels:?}");
        assert!(labels.contains(&"Fail firmware installs"), "{labels:?}");
        // Not this one: nothing declared bidirectional power.
        assert!(!labels.contains(&"Toggle V2G discharge"), "{labels:?}");
        let first_hardware = labels
            .iter()
            .position(|label| *label == "Install firmware locally")
            .unwrap();
        let last_command = labels
            .iter()
            .position(|label| *label == "Plug in vehicle")
            .unwrap();
        assert!(last_command < first_hardware, "{labels:?}");
    }

    /// One installer tracks one installation, so the row disappears while a campaign is running -
    /// asserted here through the same `campaigns` field a real snapshot writes.
    #[test]
    fn starting_a_second_firmware_update_is_not_offered_while_one_is_in_flight() {
        let (mut app, _receiver) = campaign_app();
        app.campaigns.firmware_install = Some(FirmwareInstallStage::Installing);

        let labels: Vec<&str> = app
            .palette_entries()
            .iter()
            .map(|entry| entry.label())
            .collect();

        assert!(!labels.contains(&"Install firmware locally"), "{labels:?}");
        // Arming a failure still is: that is how you fail the install already running.
        assert!(labels.contains(&"Fail firmware installs"), "{labels:?}");
    }

    /// The end-to-end proof, on a real charger thread: dispatching a local firmware update runs the
    /// download and the install against the actual hardware in the bundle, and the campaign strip's
    /// data comes back through the snapshot. The one test here that exercises
    /// `drive_running_charger`'s spawned campaign task, `FakeFileTransfer::run_download` and
    /// `FakeFirmwareInstaller::run_install` together - everything else in this section stubs the
    /// channel.
    ///
    /// Simulated time is forwarded in 10s slices through the same `tick_metrics_with` the main loop
    /// uses, so the 20s download and 30s install resolve in no wall-clock time at all. Bounded by a
    /// deadline: a regression fails this rather than hanging it.
    #[test]
    fn a_locally_dispatched_firmware_update_actually_runs_on_the_real_hardware() {
        let mut entry = charger("CP-FW-LIVE");
        entry.config.capabilities.firmware_management = true;
        let mut app = App::new(vec![entry]);
        app.confirm_charger_selection();

        // Wait for the charger's first snapshot, which is what tells the palette the installer
        // exists (`Some(Idle)` rather than `None`).
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.campaigns.firmware_install.is_none() {
            app.drain_charger_snapshots();
            assert!(Instant::now() < deadline, "no snapshot from the charger");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            app.campaigns.firmware_install,
            Some(FirmwareInstallStage::Idle),
            "the installer is there, with nothing to do yet"
        );

        dispatch_from_palette(&mut app, "install firmware");

        let mut saw_download = false;
        let mut saw_installing = false;
        // Generous on purpose: the work itself is instant (simulated time is forwarded in 10s
        // slices below), so this bounds only how long a *regression* is allowed to hang, and a
        // loaded machine running the whole suite in parallel must not trip it.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            app.drain_charger_snapshots();
            saw_download |= app.campaigns.firmware_download.is_some();
            saw_installing |=
                app.campaigns.firmware_install == Some(FirmwareInstallStage::Installing);
            if app.campaigns.firmware_install == Some(FirmwareInstallStage::Installed) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the local firmware update never completed: download seen: {saw_download}, \
                 installing seen: {saw_installing}, stage: {:?}",
                app.campaigns.firmware_install
            );
            app.tick_metrics_with(Duration::from_secs(10), Instant::now());
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(
            saw_download,
            "the image is fetched before it is installed, and that has to be observable"
        );
        assert!(saw_installing, "so does the install itself");
    }

    #[test]
    fn returning_to_the_picker_also_drops_the_hardware_control_channel() {
        let (mut app, _receiver) = v2g_app();

        app.handle_key_event(key(KeyCode::Esc));

        assert!(app.hardware_control_sender.is_none());
    }

    // --- mouse support (Phase 7) ----------------------------------------------------------

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// A dashboard app with a charger with two EVSEs (2 connectors, then 1), on a wide-enough
    /// `last_frame_area` for the sidebar to appear - matching `handle_mouse_event`'s assumption
    /// that it's always hit-testing against the layout the last real frame used.
    fn app_with_tree(width: u16, height: u16) -> App {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 2,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.last_frame_area = Rect::new(0, 0, width, height);
        app
    }

    #[test]
    fn clicking_a_connector_row_in_the_tree_focuses_it() {
        let mut app = app_with_tree(120, 40);
        // header (2) + tree section's top rule (1): EVSE 1 summary is the tree's first content
        // row, connector 2 (evse 0, connector 1) is the row right after it.
        let tree_top = crate::ui::dashboard::dashboard_layout(app.last_frame_area)
            .body
            .y;
        let click_row = tree_top + 1 /* section top rule */ + 2 /* EVSE summary, C1 */;

        app.handle_mouse_event(mouse(MouseEventKind::Down(MouseButton::Left), 5, click_row));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 0,
                connector: 1
            }
        );
    }

    #[test]
    fn clicking_an_evse_summary_row_does_not_change_focus() {
        let mut app = app_with_tree(120, 40);
        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };
        let tree_top = crate::ui::dashboard::dashboard_layout(app.last_frame_area)
            .body
            .y;
        let click_row = tree_top + 1; // the EVSE 1 summary row itself

        app.handle_mouse_event(mouse(MouseEventKind::Down(MouseButton::Left), 5, click_row));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 0,
                connector: 1
            }
        );
    }

    #[test]
    fn clicking_outside_the_tree_does_nothing() {
        let mut app = app_with_tree(120, 40);
        let original = app.focused;

        app.handle_mouse_event(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));

        assert_eq!(app.focused, original);
    }

    #[test]
    fn scrolling_the_wheel_over_the_log_pane_pauses_and_moves_the_view() {
        let mut app = app_with_logs(20);
        app.last_frame_area = Rect::new(0, 0, 120, 40);
        let log_area = crate::ui::dashboard::dashboard_layout(app.last_frame_area).log;

        assert!(!app.logs.is_paused());
        app.handle_mouse_event(mouse(MouseEventKind::ScrollUp, 5, log_area.y + 1));
        assert!(app.logs.is_paused());
        let offset_after_up = app.logs.scroll_offset();
        assert!(offset_after_up > 0);

        app.handle_mouse_event(mouse(MouseEventKind::ScrollDown, 5, log_area.y + 1));
        assert_eq!(app.logs.scroll_offset(), offset_after_up - 1);
    }

    #[test]
    fn scrolling_the_wheel_outside_the_log_pane_does_not_scroll_it() {
        let mut app = app_with_logs(20);
        app.last_frame_area = Rect::new(0, 0, 120, 40);

        app.handle_mouse_event(mouse(MouseEventKind::ScrollUp, 5, 0));

        assert!(!app.logs.is_paused());
    }

    #[test]
    fn clicking_a_palette_row_selects_it_without_dispatching() {
        let mut app = app_with_logs(0);
        app.last_frame_area = Rect::new(0, 0, 120, 40);
        app.open_command_palette();
        let commands = app.palette_commands();
        assert!(
            commands.len() >= 2,
            "need at least two commands to click the second one: {commands:?}"
        );

        let popup = crate::ui::palette::palette_popup_rect(app.last_frame_area, commands.len());
        let list_area = crate::ui::palette::palette_list_area(popup);
        let second_row = (list_area.x + 1, list_area.y + 2);

        app.handle_mouse_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            second_row.0,
            second_row.1,
        ));

        assert_eq!(app.command_palette_selected, 1);
        // Still open - a click only moves the selection, the same as ↑/↓; Enter is what
        // actually dispatches (see `handle_command_palette_mouse`'s doc comment).
        assert!(app.command_palette_open);
    }

    #[test]
    fn mouse_events_are_ignored_while_the_quit_confirm_is_open() {
        let mut app = app_with_tree(120, 40);
        app.quit_confirm_open = true;
        let original = app.focused;

        let tree_top = crate::ui::dashboard::dashboard_layout(app.last_frame_area)
            .body
            .y;
        app.handle_mouse_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            5,
            tree_top + 1 + 2,
        ));

        assert_eq!(app.focused, original);
    }
}
