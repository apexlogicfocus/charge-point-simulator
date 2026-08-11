//! A [`ring`]-backed [`SoftwareCrypto`] - the crypto half [`super::keys::FileKeyStore`] was left
//! generic over, per `docs/hardware-roadmap.md` decision 5. `ring` is already in the dependency
//! tree transitively (via `rustls` in the websocket stack), so this adds no new supply-chain
//! surface, and it is the most-audited pure-Rust option available. This is the one module in the
//! crate that does real asymmetric cryptography - see [`super::keys`] and [`super::iso15118`] for
//! why everything around it stays deliberately opaque instead.
//!
//! # What is real here
//!
//! [`RingCrypto::generate_key_pair`] generates an actual ECDSA keypair
//! ([`ring::signature::EcdsaKeyPair::generate_pkcs8`], PKCS#8-encoded, backed by `ring`'s own CSPRNG
//! via [`ring::rand::SystemRandom`]) - not a placeholder byte pattern like the test-only `FakeCrypto`
//! in [`super::keys`]'s tests. [`RingCrypto::sign`] and the free function [`verify_signature`] are
//! real ECDSA signing and verification over the P-256 and P-384 curves, using `ring`'s own
//! field/curve arithmetic throughout. No primitive here is hand-implemented; everything bottoms out
//! in a `ring` call.
//!
//! # What `ring` cannot do, and what that means for this backend
//!
//! Two real limitations of `ring`'s public API shape this module. Both are documented here rather
//! than worked around with another crate or hand-rolled math, per the roadmap's instruction to stop
//! and report rather than substitute.
//!
//! 1. **No RSA key generation.** `ring` can *sign with* an RSA key already loaded from DER/PKCS#8
//!    ([`ring::signature::RsaKeyPair::from_der`]), but it has no public API to *generate* one - RSA
//!    keygen is deliberately out of scope for the crate (primality search is slow and easy to get
//!    subtly wrong, and `ring`'s maintainers have never added it). [`SignatureAlgorithm::Rsa2048Sha256`]
//!    and [`SignatureAlgorithm::Rsa3072Sha256`] are therefore never advertised by
//!    [`RingCrypto::supported_algorithms`], and [`RingCrypto::generate_key_pair`] fails closed with
//!    [`RingCryptoError::UnsupportedAlgorithm`] if asked for either - exactly the mechanism
//!    [`SoftwareCrypto::supported_algorithms`]'s own docs describe a backend using to declare a
//!    subset, not a corner cut in this implementation.
//! 2. **No public "sign/verify an already-hashed digest" entry point.** `ring`'s `signature` module
//!    documents this directly: "this module does not support digesting the message to be signed
//!    separately from the public key operation." [`ring::signature::EcdsaKeyPair::sign`] and
//!    [`ring::signature::UnparsedPublicKey::verify`] both take a `message` and hash it themselves
//!    (SHA-256 for P-256, SHA-384 for P-384) as the first step of ECDSA; the private `sign_digest`/
//!    `verify_digest` entry points that would skip that internal hash exist inside `ring` but are
//!    not `pub`. [`KeyStore::sign`](ocpp_charge_point::hardware::KeyStore::sign)'s contract, however,
//!    is to sign a `digest` the *caller* already hashed (see `ocpp-charge-point`'s
//!    `certificates/csr.rs`, which computes `sha256(tbs)` before calling `KeyStore::sign`). Passing
//!    that already-hashed digest through `ring`'s message-signing API therefore hashes it a second
//!    time: the signature this module produces is over `SHA-256(digest)`, not `digest` directly.
//!
//!    This module's own [`RingCrypto::sign`] and [`verify_signature`] apply that second hash
//!    identically on both sides, so **signing and verifying through this module are internally
//!    consistent**: a signature this module produces always verifies through this module's own
//!    `verify_signature`, and fails closed against a different key or tampered input exactly as a
//!    correct ECDSA implementation must. What this module cannot promise is *external
//!    interoperability*: a signature produced here will not verify against an independent ECDSA
//!    implementation (a real CA checking a CSR, a
//!    peer TLS stack checking a client certificate signature during a live mutual-TLS handshake)
//!    that hashes the original message exactly once, because that peer never sees - and has no way
//!    to reproduce - this module's extra hash step. This is a genuine gap in `ring`'s public surface,
//!    not a design choice made here; wiring [`super::keys::FileKeyStore<RingCrypto>`] into anything
//!    that needs byte-for-byte interoperability with a real external verifier (mutual TLS being the
//!    concrete case in `ocpp-charge-point`'s own `mutual_tls.rs`) is out of scope for this task
//!    (registration is a separate follow-up) and should not be done without resolving this first -
//!    either an upstream `ring` change exposing digest-only signing, or a different way to supply
//!    the digest.
//!
//! # What is never logged
//!
//! Private key bytes never appear in a `tracing` call, a `Debug` impl, or a `Display` impl anywhere
//! in this module. [`RingCrypto`] derives `Debug` safely because it holds nothing but
//! [`ring::rand::SystemRandom`] (itself `Debug`, and stateless besides an OS handle) - no key
//! material is ever held on `RingCrypto` itself, only passed through per-call exactly as
//! [`SoftwareCrypto`]'s signature requires. [`RingCryptoError`] carries only algorithm identifiers,
//! never key bytes, digests, or signatures.
//!
//! # Public key encoding
//!
//! [`PublicKey::bytes`] is `ring`'s uncompressed SEC1 point encoding (`0x04 || X || Y`), exactly what
//! [`ring::signature::KeyPair::public_key`] returns for an ECDSA key pair - `ring`'s own choice, not
//! this module's. Signatures are the fixed-length (PKCS#11-style) `r || s` encoding
//! ([`ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING`] / `_P384_SHA384_FIXED_SIGNING`), chosen over
//! the ASN.1 variant because it has no encoded-length edge cases to reason about when round-tripping
//! in tests.

use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING, ECDSA_P384_SHA384_FIXED,
    ECDSA_P384_SHA384_FIXED_SIGNING, EcdsaKeyPair, EcdsaSigningAlgorithm,
    EcdsaVerificationAlgorithm, KeyPair as _, UnparsedPublicKey,
};

use ocpp_charge_point::hardware::{PublicKey, SignatureAlgorithm, SoftwareCrypto};

/// A [`SoftwareCrypto`] backend over `ring`'s ECDSA (P-256 and P-384) primitives - see the module
/// docs for exactly what is real, what `ring` cannot do, and the digest-hashing caveat that follows
/// from it.
#[derive(Debug)]
pub struct RingCrypto {
    rng: SystemRandom,
}

impl RingCrypto {
    /// A backend drawing randomness from the OS CSPRNG via [`ring::rand::SystemRandom`].
    pub fn new() -> Self {
        Self {
            rng: SystemRandom::new(),
        }
    }
}

impl Default for RingCrypto {
    fn default() -> Self {
        Self::new()
    }
}

/// The error type of [`RingCrypto`]'s operations, and of the free function [`verify_signature`].
///
/// Carries only algorithm identifiers and a coarse outcome - never key material, digests, or
/// signature bytes, per the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingCryptoError {
    /// The requested algorithm is not one this backend can generate keys for or sign/verify with -
    /// see the module docs for why RSA is never advertised.
    UnsupportedAlgorithm(SignatureAlgorithm),
    /// `ring` failed to generate a new keypair (its CSPRNG was unavailable, or key generation could
    /// not complete).
    KeyGenerationFailed,
    /// `ring` rejected key material handed back to it - a corrupt or foreign-encoded private key.
    /// [`SoftKeyStoreError::Crypto`](ocpp_charge_point::hardware::SoftKeyStoreError::Crypto) is the
    /// caller-visible wrapper.
    KeyRejected,
    /// `ring` failed to produce a signature for otherwise-valid inputs.
    SigningFailed,
    /// Signature verification failed - the signature does not match the public key and digest given.
    /// **Fail closed**: this is also what a malformed signature, a wrong key, or tampered digest
    /// bytes produce, so a caller must never treat anything other than `Ok(())` as "verified".
    VerificationFailed,
}

impl std::fmt::Display for RingCryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAlgorithm(algorithm) => {
                write!(
                    f,
                    "{algorithm:?} is not supported by the ring crypto backend"
                )
            }
            Self::KeyGenerationFailed => f.write_str("ring failed to generate a keypair"),
            Self::KeyRejected => f.write_str("ring rejected the supplied key material"),
            Self::SigningFailed => f.write_str("ring failed to produce a signature"),
            Self::VerificationFailed => f.write_str("signature verification failed"),
        }
    }
}

impl std::error::Error for RingCryptoError {}

/// Maps a [`SignatureAlgorithm`] to the `ring` signing algorithm that generates and signs with it -
/// `Err` for anything `ring` cannot generate keys for (RSA - see the module docs).
fn signing_algorithm(
    algorithm: SignatureAlgorithm,
) -> Result<&'static EcdsaSigningAlgorithm, RingCryptoError> {
    match algorithm {
        SignatureAlgorithm::EcdsaP256Sha256 => Ok(&ECDSA_P256_SHA256_FIXED_SIGNING),
        SignatureAlgorithm::EcdsaP384Sha384 => Ok(&ECDSA_P384_SHA384_FIXED_SIGNING),
        SignatureAlgorithm::Rsa2048Sha256 | SignatureAlgorithm::Rsa3072Sha256 => {
            Err(RingCryptoError::UnsupportedAlgorithm(algorithm))
        }
    }
}

/// Maps a [`SignatureAlgorithm`] to the `ring` verification algorithm matching
/// [`signing_algorithm`]'s choice for the same algorithm.
fn verification_algorithm(
    algorithm: SignatureAlgorithm,
) -> Result<&'static EcdsaVerificationAlgorithm, RingCryptoError> {
    match algorithm {
        SignatureAlgorithm::EcdsaP256Sha256 => Ok(&ECDSA_P256_SHA256_FIXED),
        SignatureAlgorithm::EcdsaP384Sha384 => Ok(&ECDSA_P384_SHA384_FIXED),
        SignatureAlgorithm::Rsa2048Sha256 | SignatureAlgorithm::Rsa3072Sha256 => {
            Err(RingCryptoError::UnsupportedAlgorithm(algorithm))
        }
    }
}

impl SoftwareCrypto for RingCrypto {
    type Error = RingCryptoError;

    fn generate_key_pair(
        &self,
        algorithm: SignatureAlgorithm,
    ) -> Result<(Vec<u8>, PublicKey), Self::Error> {
        let alg = signing_algorithm(algorithm)?;

        // `generate_pkcs8` gives us only the encoded document; reload it to reach the public key
        // through `ring::signature::KeyPair`, mirroring `ring`'s own Ed25519 example in
        // `ring::signature`'s module docs.
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(alg, &self.rng)
            .map_err(|_| RingCryptoError::KeyGenerationFailed)?;
        let key_pair = EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &self.rng)
            .map_err(|_| RingCryptoError::KeyRejected)?;

        let public_key = PublicKey {
            algorithm,
            bytes: key_pair.public_key().as_ref().to_vec(),
        };
        Ok((pkcs8.as_ref().to_vec(), public_key))
    }

    fn sign(
        &self,
        algorithm: SignatureAlgorithm,
        private_key: &[u8],
        digest: &[u8],
    ) -> Result<Vec<u8>, Self::Error> {
        let alg = signing_algorithm(algorithm)?;
        let key_pair = EcdsaKeyPair::from_pkcs8(alg, private_key, &self.rng)
            .map_err(|_| RingCryptoError::KeyRejected)?;
        let signature = key_pair
            .sign(&self.rng, digest)
            .map_err(|_| RingCryptoError::SigningFailed)?;
        Ok(signature.as_ref().to_vec())
    }

    fn supported_algorithms(&self) -> &[SignatureAlgorithm] {
        // RSA is deliberately absent - `ring` cannot generate an RSA keypair. See the module docs.
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
/// that needs to verify a signature produced by this same backend, subject to the digest-hashing
/// caveat in the module docs.
///
/// **Fails closed on every error path**: an unsupported algorithm, a malformed public key, a
/// tampered digest, or a tampered signature all produce `Err`, never `Ok(())`.
pub fn verify_signature(
    algorithm: SignatureAlgorithm,
    public_key: &[u8],
    digest: &[u8],
    signature: &[u8],
) -> Result<(), RingCryptoError> {
    let alg = verification_algorithm(algorithm)?;
    UnparsedPublicKey::new(alg, public_key)
        .verify(digest, signature)
        .map_err(|_| RingCryptoError::VerificationFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::hardware::{FileKeyStore, FileStorage};
    use ocpp_charge_point::hardware::{KeyStore, SoftKeyStoreError};

    #[test]
    fn a_generated_key_round_trips_through_sign_then_verify() {
        let crypto = RingCrypto::new();
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
        let crypto = RingCrypto::new();
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

    #[test]
    fn verification_fails_closed_against_a_tampered_digest() {
        let crypto = RingCrypto::new();
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
        assert_eq!(result, Err(RingCryptoError::VerificationFailed));
    }

    #[test]
    fn verification_fails_closed_against_the_wrong_public_key() {
        let crypto = RingCrypto::new();
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
        assert_eq!(result, Err(RingCryptoError::VerificationFailed));
    }

    #[test]
    fn verification_fails_closed_against_a_tampered_signature() {
        let crypto = RingCrypto::new();
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
        assert_eq!(result, Err(RingCryptoError::VerificationFailed));
    }

    #[test]
    fn rsa_algorithms_are_not_advertised_as_supported() {
        let crypto = RingCrypto::new();
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
        let crypto = RingCrypto::new();
        let result = crypto.generate_key_pair(SignatureAlgorithm::Rsa2048Sha256);
        assert_eq!(
            result.unwrap_err(),
            RingCryptoError::UnsupportedAlgorithm(SignatureAlgorithm::Rsa2048Sha256)
        );
    }

    #[tokio::test]
    async fn keys_persisted_through_a_file_key_store_survive_a_fresh_store_over_the_same_directory()
    {
        let dir = tempfile::tempdir().unwrap();
        let generated = {
            let before = FileKeyStore::new(FileStorage::new(dir.path()), RingCrypto::new());
            before
                .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
                .await
                .unwrap()
        };

        // A fresh store and a fresh `RingCrypto` (nothing but a `SystemRandom` handle, so nothing
        // to carry over) - as a restart would create - over the same directory on disk.
        let after = FileKeyStore::new(FileStorage::new(dir.path()), RingCrypto::new());
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
        let store = FileKeyStore::with_limit(FileStorage::new(dir.path()), RingCrypto::new(), 1);

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
        // Proves the whole composition end to end: `FileKeyStore` over `RingCrypto`, generating,
        // persisting, and signing with a real key - the shape a CSR (B4.3) or a contract-certificate
        // key would actually be used through.
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::new(FileStorage::new(dir.path()), RingCrypto::new());

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

    /// A compile-time check that `RingCrypto` satisfies the bounds
    /// [`super::super::keys::FileKeyStore`] (and eventually a registration) needs -
    /// `SoftwareCrypto + Send + Sync + 'static` - mirroring every other fake's own bound-check test
    /// in this module directory.
    #[allow(dead_code)]
    fn assert_satisfies_the_key_store_bounds<T: SoftwareCrypto + Send + Sync + 'static>() {}
    #[allow(dead_code)]
    fn ring_crypto_satisfies_the_key_store_bounds() {
        assert_satisfies_the_key_store_bounds::<RingCrypto>();
    }
}
