//! A [`CertificateStore`] persisted through [`FileStorage`], so a simulated charger's installed
//! certificates (CSMS roots, its own signed client certificate, ...) survive a process restart -
//! exactly the property [`super::storage`] exists to give every other piece of persisted state.
//!
//! # What is composed versus written here
//!
//! Upstream already ships [`StoredCertificates`], a complete [`CertificateStore`] over any
//! [`Storage`](ocpp_charge_point::hardware::Storage) implementation - bounded, persistence-backed,
//! and fully tested on its own side. This module does no certificate handling of its own: it wraps
//! [`StoredCertificates<FileStorage>`] and adds two things that are this simulator's job, not
//! upstream's -
//!
//! 1. Wiring the storage: [`FileCertificateStore`] fixes the generic storage parameter to
//!    [`FileStorage`], so a caller only ever has to think about *which directory*, matching how
//!    [`super::display::FakeDisplay`] and [`super::connector::FakeConnector`] hide their own
//!    plumbing behind a plain constructor.
//! 2. Observability: every mutating call logs an outcome via `tracing`, the same way every other
//!    fake hardware type in this module does, so a CSMS developer watching the simulator's log
//!    output (e.g. the TUI's log panel) sees a certificate operation happen the moment it does.
//!    Only identifiers and outcomes are logged, never certificate bytes or key material.
//!
//! # An honest limitation this store inherits, not introduces
//!
//! [`CertificateStore::install`] - the method a real `InstallCertificate` OCPP message reaches -
//! **always returns [`InstallCertificateOutcome::Rejected`] for a CSMS-pushed root**
//! ([`CertificateUse::is_installable`] uses). That is not a bug or an artificial restriction added
//! here: [`StoredCertificates`] has no X.509 parser (this crate carries no crypto dependency at
//! all, [`CertificateStore`]'s own module docs explain why), so it cannot compute the
//! `issuerNameHash`/`issuerKeyHash`/`serialNumber` a CSMS would later address the certificate by -
//! storing one anyway would make it undeletable. A CSMS developer who wants to drive an *accepted*
//! `InstallCertificate` end to end must go through [`FileCertificateStore::install_with_hash`]
//! (mirroring [`StoredCertificates::install_with_hash`]) with hashes computed elsewhere, or treat
//! the always-`Rejected` path itself as the thing under test - both are legitimate simulator uses,
//! and the second is exactly the "installation is rejected" failure path CSMS integrations need to
//! exercise. The charge point's *own* certificate (arriving via `CertificateSigned`, not
//! `InstallCertificate`) is unaffected: [`CertificateStore::install`]'s other branch stores it with
//! a self-computed stand-in identity, as documented on [`StoredCertificates::install`].

use ocpp_charge_point::hardware::{
    CertificateHashData, CertificateStore, CertificateUse, DeleteCertificateOutcome,
    InstallCertificateOutcome, InstalledCertificate, StoredCertificates, StoredCertificatesError,
};

use super::storage::FileStorage;

/// A [`CertificateStore`] backed by [`FileStorage`] - see the module docs for what is composed
/// from upstream's [`StoredCertificates`] versus added here.
pub struct FileCertificateStore {
    inner: StoredCertificates<FileStorage>,
}

impl std::fmt::Debug for FileCertificateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileCertificateStore")
            .finish_non_exhaustive()
    }
}

impl FileCertificateStore {
    /// A store over `storage`, holding at most
    /// [`DEFAULT_MAX_CERTIFICATES`](ocpp_charge_point::hardware::DEFAULT_MAX_CERTIFICATES) -
    /// real hardware has finite slots, and a store a remote CSMS could grow without bound would
    /// not be simulating that.
    pub fn new(storage: FileStorage) -> Self {
        Self {
            inner: StoredCertificates::new(storage),
        }
    }

    /// A store over `storage`, holding at most `max_certificates` (clamped to at least one by
    /// [`StoredCertificates::with_limit`]).
    pub fn with_limit(storage: FileStorage, max_certificates: usize) -> Self {
        Self {
            inner: StoredCertificates::with_limit(storage, max_certificates),
        }
    }

    /// Stores `certificate` under hashes the caller has already computed - the way to drive an
    /// *accepted* install for a CSMS-pushed root, since [`CertificateStore::install`] cannot
    /// compute them itself (see the module docs). Mirrors
    /// [`StoredCertificates::install_with_hash`] plus this store's own `tracing`.
    pub async fn install_with_hash(
        &self,
        use_for: CertificateUse,
        certificate: &str,
        hash_data: CertificateHashData,
    ) -> InstallCertificateOutcome {
        let serial_number = hash_data.serial_number.clone();
        let outcome = self
            .inner
            .install_with_hash(use_for, certificate, hash_data)
            .await;
        tracing::info!(
            ?use_for,
            serial_number,
            ?outcome,
            "certificate install (with hash) requested"
        );
        outcome
    }
}

#[async_trait::async_trait]
impl CertificateStore for FileCertificateStore {
    type Error = StoredCertificatesError;

    async fn install(
        &self,
        use_for: CertificateUse,
        certificate: &str,
    ) -> Result<InstallCertificateOutcome, Self::Error> {
        let outcome = self.inner.install(use_for, certificate).await?;
        tracing::info!(?use_for, ?outcome, "certificate install requested");
        Ok(outcome)
    }

    async fn delete(
        &self,
        hash_data: &CertificateHashData,
    ) -> Result<DeleteCertificateOutcome, Self::Error> {
        let outcome = self.inner.delete(hash_data).await?;
        tracing::info!(
            serial_number = %hash_data.serial_number,
            ?outcome,
            "certificate delete requested"
        );
        Ok(outcome)
    }

    async fn installed(
        &self,
        uses: &[CertificateUse],
    ) -> Result<Vec<InstalledCertificate>, Self::Error> {
        self.inner.installed(uses).await
    }

    async fn has_client_private_key(&self) -> Result<bool, Self::Error> {
        self.inner.has_client_private_key().await
    }

    async fn certificate_chain_pem(
        &self,
        use_for: CertificateUse,
    ) -> Result<Option<String>, Self::Error> {
        self.inner.certificate_chain_pem(use_for).await
    }

    async fn all_certificate_chain_pems(
        &self,
        use_for: CertificateUse,
    ) -> Result<Vec<String>, Self::Error> {
        self.inner.all_certificate_chain_pems(use_for).await
    }

    async fn expires_at(
        &self,
        use_for: CertificateUse,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
        self.inner.expires_at(use_for).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocpp_charge_point::hardware::HashAlgorithm;

    fn hash(serial: &str) -> CertificateHashData {
        CertificateHashData {
            hash_algorithm: HashAlgorithm::Sha256,
            issuer_name_hash: "issuer-name-hash".to_string(),
            issuer_key_hash: "issuer-key-hash".to_string(),
            serial_number: serial.to_string(),
        }
    }

    fn store_over(dir: &std::path::Path) -> FileCertificateStore {
        FileCertificateStore::new(FileStorage::new(dir))
    }

    #[tokio::test]
    async fn an_installed_certificate_is_returned_by_enumeration() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());

        assert_eq!(
            store
                .install_with_hash(CertificateUse::CsmsRoot, "-----BEGIN-----", hash("01"))
                .await,
            InstallCertificateOutcome::Accepted
        );

        let installed = store.installed(&[]).await.unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].use_for, CertificateUse::CsmsRoot);
        assert_eq!(installed[0].hash_data.serial_number, "01");
    }

    #[tokio::test]
    async fn deleting_an_installed_certificate_removes_it_and_a_second_delete_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());
        store
            .install_with_hash(CertificateUse::CsmsRoot, "-----BEGIN-----", hash("01"))
            .await;

        assert_eq!(
            store.delete(&hash("01")).await.unwrap(),
            DeleteCertificateOutcome::Accepted
        );
        assert!(store.installed(&[]).await.unwrap().is_empty());

        // The CSMS wanted it gone and it already is - not an error.
        assert_eq!(
            store.delete(&hash("01")).await.unwrap(),
            DeleteCertificateOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn deleting_something_never_installed_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());

        assert_eq!(
            store.delete(&hash("99")).await.unwrap(),
            DeleteCertificateOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn certificates_survive_a_fresh_store_over_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        {
            let before = store_over(dir.path());
            before
                .install_with_hash(CertificateUse::CsmsRoot, "-----BEGIN-----", hash("01"))
                .await;
        }

        // A fresh store, as a restart would create, over the same directory on disk.
        let after = store_over(dir.path());
        let recovered = after.installed(&[]).await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].hash_data.serial_number, "01");
    }

    #[tokio::test]
    async fn exceeding_the_maximum_is_rejected_rather_than_silently_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCertificateStore::with_limit(FileStorage::new(dir.path()), 1);

        assert_eq!(
            store
                .install_with_hash(CertificateUse::CsmsRoot, "a", hash("01"))
                .await,
            InstallCertificateOutcome::Accepted
        );
        assert_eq!(
            store
                .install_with_hash(CertificateUse::V2gRoot, "b", hash("02"))
                .await,
            InstallCertificateOutcome::Failed
        );
        // Never a silent overwrite: the first entry is still exactly what it was.
        let installed = store.installed(&[]).await.unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].hash_data.serial_number, "01");
    }

    #[tokio::test]
    async fn a_csms_pushed_root_through_plain_install_is_honestly_rejected() {
        // No X.509 parser means no way to compute the hash a CSMS would later address this
        // certificate by - see the module docs. This is the failure path a CSMS developer needs
        // to be able to exercise reproducibly, not a bug.
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());

        assert_eq!(
            store
                .install(CertificateUse::CsmsRoot, "-----BEGIN CERTIFICATE-----")
                .await,
            Ok(InstallCertificateOutcome::Rejected)
        );
        assert!(store.installed(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_charge_points_own_certificate_is_accepted_through_plain_install() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());

        assert_eq!(
            store
                .install(
                    CertificateUse::ChargingStation,
                    "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----",
                )
                .await,
            Ok(InstallCertificateOutcome::Accepted)
        );
        assert_eq!(
            store
                .certificate_chain_pem(CertificateUse::ChargingStation)
                .await
                .unwrap(),
            Some("-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----".to_string())
        );
    }

    #[tokio::test]
    async fn a_flash_backed_store_never_claims_to_hold_a_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());
        assert!(!store.has_client_private_key().await.unwrap());
    }

    #[tokio::test]
    async fn listing_filters_by_use_and_an_empty_filter_means_all() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());
        store
            .install_with_hash(CertificateUse::CsmsRoot, "a", hash("01"))
            .await;
        store
            .install_with_hash(CertificateUse::V2gRoot, "b", hash("02"))
            .await;

        assert_eq!(store.installed(&[]).await.unwrap().len(), 2);
        let csms = store.installed(&[CertificateUse::CsmsRoot]).await.unwrap();
        assert_eq!(csms.len(), 1);
        assert_eq!(csms[0].use_for, CertificateUse::CsmsRoot);
    }

    /// The registration this drives (a follow-up task) will require `CertificateStore + Send +
    /// Sync + 'static` - a compile-time check that `FileCertificateStore` satisfies those bounds
    /// now, mirroring `FakeDisplay`'s own bound-check test in `display.rs`.
    #[allow(dead_code)]
    fn assert_satisfies_the_builder_bounds<T: CertificateStore + Send + Sync + 'static>() {}

    #[allow(dead_code)]
    fn file_certificate_store_satisfies_the_builder_bounds() {
        assert_satisfies_the_builder_bounds::<FileCertificateStore>();
    }
}
