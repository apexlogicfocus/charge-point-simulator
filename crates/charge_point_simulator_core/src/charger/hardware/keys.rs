//! A [`KeyStore`] persisted through [`FileStorage`], composing upstream's [`SoftKeyStore`] the way
//! [`super::certificates`] composes [`ocpp_charge_point::hardware::StoredCertificates`] - see that
//! module's docs for the same shape applied to certificates.
//!
//! # Why this module is generic, and what it deliberately does not ship
//!
//! [`SoftKeyStore`] needs two things: a [`Storage`](ocpp_charge_point::hardware::Storage) to
//! persist key material in, and a [`SoftwareCrypto`] backend that actually generates keypairs and
//! produces signatures. This module supplies the first - [`FileKeyStore<C>`] fixes it to
//! [`FileStorage`], the same wiring [`super::certificates::FileCertificateStore`] does - but
//! **deliberately leaves the second as a type parameter it does not choose a default for.**
//!
//! `ocpp-charge-point` ships the [`SoftwareCrypto`] *trait* and documents it as "the pluggable
//! crypto backend `SoftKeyStore` itself needs" (see `docs/INTEGRATORS.md` in the vendored crate),
//! but carries no crypto dependency of its own and therefore no concrete implementation - the same
//! "no crypto dependency" stance [`ocpp_charge_point::hardware::CertificateStore`]'s module docs
//! give for why that trait does no X.509 parsing either. Real asymmetric key generation and
//! signing (ECDSA/RSA) is exactly the kind of primitive this project's working agreements say not
//! to invent: "a simulator with homegrown crypto is worse than one that honestly reports the gap."
//! So this module composes the storage half, which is genuinely just wiring, and stops there for
//! the crypto half rather than shipping a fake that would silently produce signatures nothing
//! could ever verify.
//!
//! **This means no concrete, ready-to-use software key store exists in this simulator yet.**
//! Wiring one in - which needs an actual decision about a crypto backend - is left to whoever picks
//! up plug-and-charge registration, the same way `charger/hardware_bundle.rs` registration is out
//! of scope for this task. [`ocpp_charge_point::hardware::NoKeyStore`] remains the honest default
//! in the meantime: it never claims to hold a key it doesn't.
//!
//! The tests below use a small `FakeCrypto` (in `#[cfg(test)]` only, mirroring the private one
//! `ocpp-charge-point` keeps in its own `key_storage.rs` tests) to prove the *storage* half of this
//! composition - persistence, the key limit, not-found handling - actually works. It performs no
//! real cryptographic operation and must never be used outside a test.

use ocpp_charge_point::hardware::{
    GeneratedKeyPair, KeyHandle, KeyStore, KeyStoreBacking, SignatureAlgorithm, SoftKeyStore,
    SoftKeyStoreError, SoftwareCrypto,
};

use super::storage::FileStorage;

/// A [`KeyStore`] backed by [`FileStorage`] for persistence, generic over the [`SoftwareCrypto`]
/// backend that performs the actual key generation and signing - see the module docs for why no
/// default is provided.
pub struct FileKeyStore<C> {
    inner: SoftKeyStore<FileStorage, C>,
}

impl<C> std::fmt::Debug for FileKeyStore<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileKeyStore").finish_non_exhaustive()
    }
}

impl<C: SoftwareCrypto> FileKeyStore<C> {
    /// A store over `storage` and `crypto`, holding at most
    /// [`DEFAULT_MAX_KEYS`](ocpp_charge_point::hardware::DEFAULT_MAX_KEYS) - real hardware has
    /// finite key slots, and a store a remote CSMS could grow without bound would not be
    /// simulating that.
    pub fn new(storage: FileStorage, crypto: C) -> Self {
        Self {
            inner: SoftKeyStore::new(storage, crypto),
        }
    }

    /// A store over `storage` and `crypto`, holding at most `max_keys` (clamped to at least one by
    /// [`SoftKeyStore::with_limit`]).
    pub fn with_limit(storage: FileStorage, crypto: C, max_keys: usize) -> Self {
        Self {
            inner: SoftKeyStore::with_limit(storage, crypto, max_keys),
        }
    }
}

#[async_trait::async_trait]
impl<C> KeyStore for FileKeyStore<C>
where
    C: SoftwareCrypto + Send + Sync,
{
    type Error = SoftKeyStoreError<C::Error>;

    async fn generate_key_pair(
        &self,
        algorithm: SignatureAlgorithm,
    ) -> Result<GeneratedKeyPair, Self::Error> {
        let result = self.inner.generate_key_pair(algorithm).await;
        // Only the algorithm and the resulting handle are logged - both are identifiers, never
        // key material (see `KeyHandle`'s own docs: it "never contains key material").
        match &result {
            Ok(generated) => {
                tracing::info!(?algorithm, handle = ?generated.handle, "key pair generated");
            }
            Err(error) => {
                tracing::warn!(?algorithm, %error, "key pair generation failed");
            }
        }
        result
    }

    async fn sign(&self, handle: &KeyHandle, digest: &[u8]) -> Result<Vec<u8>, Self::Error> {
        let result = self.inner.sign(handle, digest).await;
        // Never logs the digest or the resulting signature bytes, only the handle (an opaque
        // identifier, safe to log per `KeyHandle`'s docs) and the digest's length.
        match &result {
            Ok(_) => tracing::info!(?handle, digest_len = digest.len(), "digest signed"),
            Err(error) => tracing::warn!(?handle, %error, "signing failed"),
        }
        result
    }

    async fn delete_key_pair(&self, handle: &KeyHandle) -> Result<(), Self::Error> {
        let result = self.inner.delete_key_pair(handle).await;
        tracing::info!(?handle, ok = result.is_ok(), "key pair delete requested");
        result
    }

    async fn supported_algorithms(&self) -> Result<Vec<SignatureAlgorithm>, Self::Error> {
        self.inner.supported_algorithms().await
    }

    async fn backing(&self) -> Result<KeyStoreBacking, Self::Error> {
        self.inner.backing().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    /// **Not real cryptography.** Proves the storage half of [`FileKeyStore`]'s composition -
    /// persistence, the key limit, fail-closed lookups - without any actual asymmetric math,
    /// mirroring the private `FakeCrypto` `ocpp-charge-point` keeps in its own `key_storage.rs`
    /// tests (inaccessible from here, so this is a from-scratch equivalent, not a copy). Must
    /// never be used outside a test - see the module docs.
    #[derive(Debug, Default)]
    struct FakeCrypto {
        next_key_byte: AtomicU8,
    }

    impl SoftwareCrypto for FakeCrypto {
        type Error = std::convert::Infallible;

        fn generate_key_pair(
            &self,
            algorithm: SignatureAlgorithm,
        ) -> Result<(Vec<u8>, ocpp_charge_point::hardware::PublicKey), Self::Error> {
            let byte = self.next_key_byte.fetch_add(1, Ordering::SeqCst);
            let private_key = vec![byte];
            let public_key = ocpp_charge_point::hardware::PublicKey {
                algorithm,
                bytes: vec![byte, byte],
            };
            Ok((private_key, public_key))
        }

        fn sign(
            &self,
            _algorithm: SignatureAlgorithm,
            private_key: &[u8],
            digest: &[u8],
        ) -> Result<Vec<u8>, Self::Error> {
            let mut signature = private_key.to_vec();
            signature.extend_from_slice(digest);
            Ok(signature)
        }

        fn supported_algorithms(&self) -> &[SignatureAlgorithm] {
            &[
                SignatureAlgorithm::EcdsaP256Sha256,
                SignatureAlgorithm::EcdsaP384Sha384,
            ]
        }
    }

    fn store_over(dir: &std::path::Path) -> FileKeyStore<FakeCrypto> {
        FileKeyStore::new(FileStorage::new(dir), FakeCrypto::default())
    }

    #[tokio::test]
    async fn a_generated_key_round_trips_through_signing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());

        let generated = store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();
        assert_eq!(
            generated.public_key.algorithm,
            SignatureAlgorithm::EcdsaP256Sha256
        );

        let signature = store.sign(&generated.handle, b"a digest").await.unwrap();
        assert!(signature.ends_with(b"a digest"));
    }

    #[tokio::test]
    async fn deleting_a_key_makes_it_unusable_and_a_second_delete_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());
        let generated = store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        store.delete_key_pair(&generated.handle).await.unwrap();
        let result = store.sign(&generated.handle, b"digest").await;
        assert!(matches!(result, Err(SoftKeyStoreError::KeyNotFound)));

        // The caller wanted it gone and it already is - not an error.
        store.delete_key_pair(&generated.handle).await.unwrap();
    }

    #[tokio::test]
    async fn signing_with_an_unknown_handle_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());

        let result = store
            .sign(&KeyHandle::new(*b"never-generated"), b"digest")
            .await;
        assert!(matches!(result, Err(SoftKeyStoreError::KeyNotFound)));
    }

    #[tokio::test]
    async fn keys_survive_a_fresh_store_over_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let generated = {
            let before = store_over(dir.path());
            before
                .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
                .await
                .unwrap()
        };

        // A fresh store, as a restart would create - a fresh `FakeCrypto` stands in for
        // reconnecting to the same secure hardware, exactly as upstream's own reboot test does.
        let after = store_over(dir.path());
        let signature = after.sign(&generated.handle, b"digest").await.unwrap();
        assert!(signature.ends_with(b"digest"));
    }

    #[tokio::test]
    async fn exceeding_the_maximum_is_rejected_rather_than_silently_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            FileKeyStore::with_limit(FileStorage::new(dir.path()), FakeCrypto::default(), 1);

        store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        let result = store
            .generate_key_pair(SignatureAlgorithm::EcdsaP384Sha384)
            .await;
        assert!(matches!(result, Err(SoftKeyStoreError::StoreFull)));
    }

    #[tokio::test]
    async fn a_software_store_never_claims_to_be_hardware_backed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());
        assert_eq!(store.backing().await.unwrap(), KeyStoreBacking::Software);
    }

    #[tokio::test]
    async fn supported_algorithms_reflects_the_crypto_backend() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_over(dir.path());
        let algorithms = store.supported_algorithms().await.unwrap();
        assert_eq!(
            algorithms,
            vec![
                SignatureAlgorithm::EcdsaP256Sha256,
                SignatureAlgorithm::EcdsaP384Sha384
            ]
        );
    }

    /// The registration a future task drives will require `KeyStore + Send + Sync + 'static` -
    /// a compile-time check that `FileKeyStore` satisfies those bounds now, mirroring
    /// `FakeDisplay`'s own bound-check test in `display.rs`.
    #[allow(dead_code)]
    fn assert_satisfies_the_builder_bounds<T: KeyStore + Send + Sync + 'static>() {}

    #[allow(dead_code)]
    fn file_key_store_satisfies_the_builder_bounds() {
        assert_satisfies_the_builder_bounds::<FileKeyStore<FakeCrypto>>();
    }
}
