mod catalog;
mod command;
mod config;
mod connect;
mod connection;
mod connection_store;
mod hardware;
mod hardware_bundle;
mod ocpp_bridge;
mod running_charger;
mod state;

pub use catalog::{
    ChargerEntry, ChargerSource, all_chargers, built_in_chargers, discover_configured_chargers,
};
pub use command::{Command, CommandParameter};
pub use config::{CapabilitiesConfig, ChargerConfig, EvseConfig, OcppVersion};
pub use connect::{connect_charger, websocket_url};
pub use connection::{ConnectionProfile, PasswordTooLong, SecurityProfile};
pub use connection_store::ConnectionStore;
pub use hardware::{
    ContractCertificate, FakeChargePoint, FakeConnector, FakeDisplay, FakeEvse, FakeFileTransfer,
    FakeFileTransferError, FakeFirmwareInstaller, FakeFirmwareInstallerError, FakeFirmwareVerifier,
    FakeFirmwareVerifierError, FakeIso15118Controller, FakeIso15118ControllerError,
    FileCertificateStore, FileKeyStore, FileStorage, FileStorageError, FirmwareInstallStage,
    RingCrypto, RingCryptoError, TransferProfile, verify_signature,
};
pub use hardware_bundle::ChargerHardware;
pub use ocpp_bridge::{apply_hardware_state, apply_ocpp_state, build_ocpp_event_for_connector};
pub use ocpp_charge_point::ConnectAndSetupError;
pub use ocpp_charge_point::state::{ChargePointEvent, ChargePointState};
pub use running_charger::{RunningCharger, start_local_charger};
pub use state::{
    ChargerState, ConnectionStatus, ConnectorState, ConnectorStatus, EvseMetrics, EvseState,
    SimulationMode, Vehicle,
};
