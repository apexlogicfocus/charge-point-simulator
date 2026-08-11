//! A RustCrypto-backed [`SoftwareCrypto`] - the crypto half [`super::keys::FileKeyStore`] was left
//! generic over, per `docs/hardware-roadmap.md` decision 5. This is the *second* backend tried
//! there: an earlier `ring`-backed one was reversed because `ring` cannot do the one operation this
//! module exists for - see "Why `ring` was rejected" below. This is the one module in the crate that
//! does real asymmetric cryptography - see [`super::keys`] and [`super::iso15118`] for why
//! everything around it stays deliberately opaque instead.
//!
//! # What is real here
//!
//! [`EcdsaCrypto::generate_key_pair`] generates an actual ECDSA keypair over NIST P-256 or P-384
//! (`p256::ecdsa::SigningKey` / `p384::ecdsa::SigningKey`, via `elliptic_curve::Generate`'s
//! `try_generate`, which draws from the OS CSPRNG through `getrandom`'s `SysRng`) - not a
//! placeholder byte pattern like the test-only `FakeCrypto` in [`super::keys`]'s tests.
//! [`EcdsaCrypto::sign`] and the free function [`verify_signature`] are real ECDSA signing
//! (deterministic, RFC 6979) and verification, using RustCrypto's own field/curve arithmetic
//! throughout - `p256`/`p384`'s `arithmetic` feature, not hand-rolled math. No primitive here is
//! hand-implemented; everything bottoms out in an `ecdsa`/`p256`/`p384` call.
//!
//! # Why `ring` was rejected, and why this backend does not repeat the mistake
//!
//! `KeyStore::sign`'s contract is to sign a `digest` the *caller* already hashed - see
//! `ocpp-charge-point`'s `certificates/csr.rs`, which computes `sha256(tbs)` before calling
//! `KeyStore::sign`. `ring`'s `signature` module has no public API for that: `EcdsaKeyPair::sign`
//! and `UnparsedPublicKey::verify` both take a `message` and hash it themselves as the first step of
//! ECDSA, so a `ring` backend passing an already-hashed digest through that API hashes it a second
//! time. The result was self-consistent (a `ring`-produced signature always verified through
//! `ring`'s own second-hashing `verify`) and completely non-interoperable with any real CA or TLS
//! peer, which hashes once. See `docs/hardware-roadmap.md` decision 5 for the full account.
//!
//! This backend uses `signature::hazmat::PrehashSigner::sign_prehash` and
//! `signature::hazmat::PrehashVerifier::verify_prehash` instead - the operation `KeyStore::sign`'s
//! contract actually models, and precisely the entry point `ring` does not expose.
//! `sign_prehash`/`verify_prehash` sign and verify exactly the bytes handed to them; nothing
//! in this module's `sign`/[`verify_signature`] hashes the digest again before calling them. The
//! module tests include `signing_signs_the_exact_digest_handed_in_not_a_second_hash_of_it`, which
//! signs a known digest and verifies it through an independently constructed `p256` verifying key
//! and signature (not this module's own [`verify_signature`]) - proving the property directly rather
//! than relying on this module's own sign and verify agreeing with each other, which is exactly the
//! symmetry that hid the `ring` bug in the first place.
//!
//! # What this backend cannot do
//!
//! **No RSA.** [`SignatureAlgorithm::Rsa2048Sha256`] and [`SignatureAlgorithm::Rsa3072Sha256`] are
//! never advertised by [`EcdsaCrypto::supported_algorithms`], and [`EcdsaCrypto::generate_key_pair`]
//! fails closed with [`EcdsaCryptoError::UnsupportedAlgorithm`] if asked for either. This mirrors the
//! previous `ring` backend's own gap (`ring` cannot generate an RSA key either), but for a different
//! reason: `docs/hardware-roadmap.md` decision 5 scopes this backend to the ECDSA algorithms the
//! crate already needed. Accepted cost: two new direct dependencies, `p256` and `p384` - fewer than
//! the roadmap's own estimate of three, because both crates re-export the `ecdsa`, `elliptic_curve`,
//! and `signature` crates they are built on, so those don't need separate `Cargo.toml` entries.
//! RustCrypto does have RSA support (the `rsa` crate) that could close this gap, but adding it is a
//! separate decision this task does not make; [`EcdsaCrypto::supported_algorithms`] is how a backend
//! is expected to declare an honest subset rather than invent support it doesn't have.
//!
//! # What is never logged
//!
//! Private key bytes never appear in a `tracing` call, a `Debug` impl, or a `Display` impl anywhere
//! in this module. [`EcdsaCrypto`] derives `Debug` safely because it is a zero-sized unit type - it
//! holds no state at all (unlike the previous `ring` backend, which held a `SystemRandom` handle;
//! `getrandom`'s `SysRng` is a stateless interface over the OS RNG, constructed fresh per call, so
//! there is nothing to carry on the struct). [`EcdsaCryptoError`] carries only algorithm identifiers,
//! never key bytes, digests, or signatures.
//!
//! # Public key and signature encoding
//!
//! [`PublicKey::bytes`] is the uncompressed SEC1 point encoding (`0x04 || X || Y`, via
//! `VerifyingKey::to_sec1_point`) - 65 bytes for P-256, 97 for P-384 - the same encoding the
//! previous `ring` backend used, so nothing downstream of `PublicKey` had to change shape.
//! Signatures are the fixed-length `r || s` encoding (via `ecdsa::Signature::to_bytes`) - 64 bytes
//! for P-256, 96 for P-384 - chosen over the ASN.1/DER variant for the same reason as before: no
//! encoded-length edge cases to reason about when round-tripping in tests.

use ocpp_charge_point::hardware::{PublicKey, SignatureAlgorithm, SoftwareCrypto};

use p256::ecdsa::signature::hazmat::{PrehashSigner as _, PrehashVerifier as _};
use p256::ecdsa::{
    Signature as P256Signature, SigningKey as P256SigningKey, VerifyingKey as P256VerifyingKey,
};
use p256::elliptic_curve::Generate as _;

use p384::ecdsa::{
    Signature as P384Signature, SigningKey as P384SigningKey, VerifyingKey as P384VerifyingKey,
};

/// A [`SoftwareCrypto`] backend over RustCrypto's ECDSA (P-256 and P-384) primitives - see the
/// module docs for exactly what is real, why it replaces the earlier `ring` backend, and the
/// encoding choices it makes.
///
/// Holds no state: `getrandom`'s `SysRng` (used for key generation) is a stateless interface over
/// the OS RNG, constructed fresh in [`Self::generate_key_pair`] rather than carried on this type.
#[derive(Debug, Clone, Copy, Default)]
pub struct EcdsaCrypto;

impl EcdsaCrypto {
    /// A backend drawing randomness from the OS CSPRNG for every key it generates.
    pub fn new() -> Self {
        Self
    }
}

/// The error type of [`EcdsaCrypto`]'s operations, and of the free function [`verify_signature`].
///
/// Carries only algorithm identifiers and a coarse outcome - never key material, digests, or
/// signature bytes, per the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcdsaCryptoError {
    /// The requested algorithm is not one this backend can generate keys for or sign/verify with -
    /// see the module docs for why RSA is never advertised.
    UnsupportedAlgorithm(SignatureAlgorithm),
    /// The OS CSPRNG failed while generating a new keypair.
    KeyGenerationFailed,
    /// The supplied private key bytes were not a valid scalar for the requested curve - a corrupt
    /// or foreign-encoded key.
    KeyRejected,
    /// RustCrypto failed to produce a signature for otherwise-valid inputs.
    SigningFailed,
    /// Signature verification failed - the signature does not match the public key and digest
    /// given, or the public key or signature bytes themselves were not validly encoded.
    /// **Fail closed**: this is also what a malformed signature, a wrong key, or tampered digest
    /// bytes produce, so a caller must never treat anything other than `Ok(())` as "verified".
    VerificationFailed,
}

impl std::fmt::Display for EcdsaCryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAlgorithm(algorithm) => {
                write!(
                    f,
                    "{algorithm:?} is not supported by the RustCrypto ECDSA backend"
                )
            }
            Self::KeyGenerationFailed => {
                f.write_str("the OS RNG failed while generating a keypair")
            }
            Self::KeyRejected => f.write_str("the supplied key material was rejected"),
            Self::SigningFailed => f.write_str("failed to produce a signature"),
            Self::VerificationFailed => f.write_str("signature verification failed"),
        }
    }
}

impl std::error::Error for EcdsaCryptoError {}

impl SoftwareCrypto for EcdsaCrypto {
    type Error = EcdsaCryptoError;

    fn generate_key_pair(
        &self,
        algorithm: SignatureAlgorithm,
    ) -> Result<(Vec<u8>, PublicKey), Self::Error> {
        match algorithm {
            SignatureAlgorithm::EcdsaP256Sha256 => {
                let signing_key = P256SigningKey::try_generate()
                    .map_err(|_| EcdsaCryptoError::KeyGenerationFailed)?;
                let verifying_key = P256VerifyingKey::from(&signing_key);
                let public_key = PublicKey {
                    algorithm,
                    bytes: verifying_key.to_sec1_point(false).as_bytes().to_vec(),
                };
                Ok((signing_key.to_bytes().to_vec(), public_key))
            }
            SignatureAlgorithm::EcdsaP384Sha384 => {
                let signing_key = P384SigningKey::try_generate()
                    .map_err(|_| EcdsaCryptoError::KeyGenerationFailed)?;
                let verifying_key = P384VerifyingKey::from(&signing_key);
                let public_key = PublicKey {
                    algorithm,
                    bytes: verifying_key.to_sec1_point(false).as_bytes().to_vec(),
                };
                Ok((signing_key.to_bytes().to_vec(), public_key))
            }
            SignatureAlgorithm::Rsa2048Sha256 | SignatureAlgorithm::Rsa3072Sha256 => {
                Err(EcdsaCryptoError::UnsupportedAlgorithm(algorithm))
            }
        }
    }

    fn sign(
        &self,
        algorithm: SignatureAlgorithm,
        private_key: &[u8],
        digest: &[u8],
    ) -> Result<Vec<u8>, Self::Error> {
        match algorithm {
            SignatureAlgorithm::EcdsaP256Sha256 => {
                let signing_key = P256SigningKey::from_slice(private_key)
                    .map_err(|_| EcdsaCryptoError::KeyRejected)?;
                let signature: P256Signature = signing_key
                    .sign_prehash(digest)
                    .map_err(|_| EcdsaCryptoError::SigningFailed)?;
                Ok(signature.to_bytes().to_vec())
            }
            SignatureAlgorithm::EcdsaP384Sha384 => {
                let signing_key = P384SigningKey::from_slice(private_key)
                    .map_err(|_| EcdsaCryptoError::KeyRejected)?;
                let signature: P384Signature = signing_key
                    .sign_prehash(digest)
                    .map_err(|_| EcdsaCryptoError::SigningFailed)?;
                Ok(signature.to_bytes().to_vec())
            }
            SignatureAlgorithm::Rsa2048Sha256 | SignatureAlgorithm::Rsa3072Sha256 => {
                Err(EcdsaCryptoError::UnsupportedAlgorithm(algorithm))
            }
        }
    }

    fn supported_algorithms(&self) -> &[SignatureAlgorithm] {
        // RSA is deliberately absent - see the module docs.
        &[
            SignatureAlgorithm::EcdsaP256Sha256,
            SignatureAlgorithm::EcdsaP384Sha384,
        ]
    }
}

/// Verifies that `signature` over `digest` was produced by the private key matching `public_key`,
/// for `algorithm`.
///
/// Not part of [`SoftwareCrypto`] - that trait has no verification method, since nothing in
/// [`super::keys::FileKeyStore`]'s own contract needs one (a caller who generated the key already
/// trusts it; verification is the *peer's* job on a real signature). This free function exists so
/// this module's own signatures can be checked - by this crate's tests, and by any future caller
/// that needs to verify a signature produced by this same backend.
///
/// **Fails closed on every error path**: an unsupported algorithm, a malformed public key, a
/// tampered digest, or a tampered signature all produce `Err`, never `Ok(())`.
pub fn verify_signature(
    algorithm: SignatureAlgorithm,
    public_key: &[u8],
    digest: &[u8],
    signature: &[u8],
) -> Result<(), EcdsaCryptoError> {
    match algorithm {
        SignatureAlgorithm::EcdsaP256Sha256 => {
            let verifying_key = P256VerifyingKey::from_sec1_bytes(public_key)
                .map_err(|_| EcdsaCryptoError::VerificationFailed)?;
            let signature = P256Signature::from_slice(signature)
                .map_err(|_| EcdsaCryptoError::VerificationFailed)?;
            verifying_key
                .verify_prehash(digest, &signature)
                .map_err(|_| EcdsaCryptoError::VerificationFailed)
        }
        SignatureAlgorithm::EcdsaP384Sha384 => {
            let verifying_key = P384VerifyingKey::from_sec1_bytes(public_key)
                .map_err(|_| EcdsaCryptoError::VerificationFailed)?;
            let signature = P384Signature::from_slice(signature)
                .map_err(|_| EcdsaCryptoError::VerificationFailed)?;
            verifying_key
                .verify_prehash(digest, &signature)
                .map_err(|_| EcdsaCryptoError::VerificationFailed)
        }
        SignatureAlgorithm::Rsa2048Sha256 | SignatureAlgorithm::Rsa3072Sha256 => {
            Err(EcdsaCryptoError::UnsupportedAlgorithm(algorithm))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::hardware::{FileKeyStore, FileStorage};
    use ocpp_charge_point::hardware::{KeyStore, SoftKeyStoreError};

    #[test]
    fn a_generated_key_round_trips_through_sign_then_verify() {
        let crypto = EcdsaCrypto::new();
        let (private_key, public_key) = crypto
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .unwrap();

        let digest = b"a digest the caller already hashed";
        let signature = crypto
            .sign(SignatureAlgorithm::EcdsaP256Sha256, &private_key, digest)
            .unwrap();

        assert_eq!(
            verify_signature(
                SignatureAlgorithm::EcdsaP256Sha256,
                &public_key.bytes,
                digest,
                &signature,
            ),
            Ok(())
        );
    }

    #[test]
    fn p384_keys_also_round_trip() {
        let crypto = EcdsaCrypto::new();
        let (private_key, public_key) = crypto
            .generate_key_pair(SignatureAlgorithm::EcdsaP384Sha384)
            .unwrap();

        let digest = b"another digest";
        let signature = crypto
            .sign(SignatureAlgorithm::EcdsaP384Sha384, &private_key, digest)
            .unwrap();

        assert_eq!(
            verify_signature(
                SignatureAlgorithm::EcdsaP384Sha384,
                &public_key.bytes,
                digest,
                &signature,
            ),
            Ok(())
        );
    }

    /// The test the module docs point to: proves [`EcdsaCrypto::sign`] signs exactly the digest it
    /// is given, not a second hash of it - the property that disqualified the previous `ring`
    /// backend (see `docs/hardware-roadmap.md` decision 5).
    ///
    /// Verification here goes through a `p256::ecdsa::VerifyingKey` and `Signature` built directly
    /// from RustCrypto's own types, decoded independently from `public_key.bytes` and the returned
    /// signature bytes - deliberately *not* this module's own [`verify_signature`] - so this test
    /// cannot pass merely because this module's own sign and verify code share a mistake. A `ring`
    /// backend that hashed the digest again before signing would fail this: the independent
    /// verifier checks the signature against `digest` exactly as given, and a signature produced
    /// over `SHA-256(digest)` does not verify against `digest` itself.
    #[test]
    fn signing_signs_the_exact_digest_handed_in_not_a_second_hash_of_it() {
        use sha2::{Digest as _, Sha256};

        let crypto = EcdsaCrypto::new();
        let (private_key, public_key) = crypto
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .unwrap();

        // A digest shaped exactly like `certificates/csr.rs` hands to `KeyStore::sign`: already
        // hashed once by the caller.
        let digest = Sha256::digest(b"a CertificationRequestInfo the caller already SHA-256'd");
        let signature = crypto
            .sign(SignatureAlgorithm::EcdsaP256Sha256, &private_key, &digest)
            .unwrap();

        // Independent verifier: RustCrypto's own types, decoded from scratch, never touching this
        // module's `verify_signature`.
        let independent_verifying_key =
            P256VerifyingKey::from_sec1_bytes(&public_key.bytes).unwrap();
        let independent_signature = P256Signature::from_slice(&signature).unwrap();

        assert!(
            independent_verifying_key
                .verify_prehash(&digest, &independent_signature)
                .is_ok(),
            "the signature must verify against the exact digest that was signed"
        );

        // The disqualifying `ring` behavior: had this module hashed `digest` again before signing
        // (as the `ring` backend was forced to), the signature would verify against
        // `SHA-256(digest)` instead - it must not.
        let double_hashed_digest = Sha256::digest(digest);
        assert!(
            independent_verifying_key
                .verify_prehash(&double_hashed_digest, &independent_signature)
                .is_err(),
            "a once-hashed digest's signature must not also verify against a second hash of it"
        );
    }

    #[test]
    fn verification_fails_closed_against_a_tampered_digest() {
        let crypto = EcdsaCrypto::new();
        let (private_key, public_key) = crypto
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .unwrap();
        let signature = crypto
            .sign(
                SignatureAlgorithm::EcdsaP256Sha256,
                &private_key,
                b"original",
            )
            .unwrap();

        let result = verify_signature(
            SignatureAlgorithm::EcdsaP256Sha256,
            &public_key.bytes,
            b"tampered",
            &signature,
        );
        assert_eq!(result, Err(EcdsaCryptoError::VerificationFailed));
    }

    #[test]
    fn verification_fails_closed_against_the_wrong_public_key() {
        let crypto = EcdsaCrypto::new();
        let (private_key, _) = crypto
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .unwrap();
        let (_, other_public_key) = crypto
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .unwrap();
        let digest = b"a digest";
        let signature = crypto
            .sign(SignatureAlgorithm::EcdsaP256Sha256, &private_key, digest)
            .unwrap();

        let result = verify_signature(
            SignatureAlgorithm::EcdsaP256Sha256,
            &other_public_key.bytes,
            digest,
            &signature,
        );
        assert_eq!(result, Err(EcdsaCryptoError::VerificationFailed));
    }

    #[test]
    fn verification_fails_closed_against_a_tampered_signature() {
        let crypto = EcdsaCrypto::new();
        let (private_key, public_key) = crypto
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .unwrap();
        let digest = b"a digest";
        let mut signature = crypto
            .sign(SignatureAlgorithm::EcdsaP256Sha256, &private_key, digest)
            .unwrap();
        signature[0] ^= 0xFF;

        let result = verify_signature(
            SignatureAlgorithm::EcdsaP256Sha256,
            &public_key.bytes,
            digest,
            &signature,
        );
        assert_eq!(result, Err(EcdsaCryptoError::VerificationFailed));
    }

    #[test]
    fn rsa_algorithms_are_not_advertised_as_supported() {
        let crypto = EcdsaCrypto::new();
        let algorithms = crypto.supported_algorithms();
        assert!(!algorithms.contains(&SignatureAlgorithm::Rsa2048Sha256));
        assert!(!algorithms.contains(&SignatureAlgorithm::Rsa3072Sha256));
        assert_eq!(
            algorithms,
            &[
                SignatureAlgorithm::EcdsaP256Sha256,
                SignatureAlgorithm::EcdsaP384Sha384
            ]
        );
    }

    #[test]
    fn requesting_an_rsa_key_pair_fails_closed_rather_than_inventing_one() {
        let crypto = EcdsaCrypto::new();
        let result = crypto.generate_key_pair(SignatureAlgorithm::Rsa2048Sha256);
        assert_eq!(
            result.unwrap_err(),
            EcdsaCryptoError::UnsupportedAlgorithm(SignatureAlgorithm::Rsa2048Sha256)
        );
    }

    #[tokio::test]
    async fn keys_persisted_through_a_file_key_store_survive_a_fresh_store_over_the_same_directory()
    {
        let dir = tempfile::tempdir().unwrap();
        let generated = {
            let before = FileKeyStore::new(FileStorage::new(dir.path()), EcdsaCrypto::new());
            before
                .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
                .await
                .unwrap()
        };

        // A fresh store and a fresh `EcdsaCrypto` (nothing but a unit type, so nothing to carry
        // over) - as a restart would create - over the same directory on disk.
        let after = FileKeyStore::new(FileStorage::new(dir.path()), EcdsaCrypto::new());
        let signature = after.sign(&generated.handle, b"digest").await.unwrap();

        assert_eq!(
            verify_signature(
                SignatureAlgorithm::EcdsaP256Sha256,
                &generated.public_key.bytes,
                b"digest",
                &signature,
            ),
            Ok(())
        );
    }

    #[tokio::test]
    async fn exceeding_the_maximum_is_rejected_with_the_real_crypto_backend() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::with_limit(FileStorage::new(dir.path()), EcdsaCrypto::new(), 1);

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
    async fn a_contract_certificate_key_installs_and_signs_through_the_full_stack() {
        // Proves the whole composition end to end: `FileKeyStore` over `EcdsaCrypto`, generating,
        // persisting, and signing with a real key - the shape a CSR (B4.3) or a contract-certificate
        // key would actually be used through.
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::new(FileStorage::new(dir.path()), EcdsaCrypto::new());

        let generated = store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();
        let digest = b"a certification request digest";
        let signature = store.sign(&generated.handle, digest).await.unwrap();

        assert_eq!(
            verify_signature(
                SignatureAlgorithm::EcdsaP256Sha256,
                &generated.public_key.bytes,
                digest,
                &signature,
            ),
            Ok(())
        );
    }

    /// A compile-time check that `EcdsaCrypto` satisfies the bounds
    /// [`super::super::keys::FileKeyStore`] (and eventually a registration) needs -
    /// `SoftwareCrypto + Send + Sync + 'static` - mirroring every other fake's own bound-check test
    /// in this module directory.
    #[allow(dead_code)]
    fn assert_satisfies_the_key_store_bounds<T: SoftwareCrypto + Send + Sync + 'static>() {}
    #[allow(dead_code)]
    fn ecdsa_crypto_satisfies_the_key_store_bounds() {
        assert_satisfies_the_key_store_bounds::<EcdsaCrypto>();
    }
}
