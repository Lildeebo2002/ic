//! # Signature API
//! The signature API contains basic, multi, and threshold signatures.
//!
//! # Threshold Signatures
//! Threshold signatures require the contribution of at least `t` out of `n`
//! participants to generate a valid signature. The key material necessary to
//! compute and verify threshold signatures is generated by running a
//! distributed key generation protocol (DKG). In particular,
//! `DkgAlgorithm::load_transcript` loads the Crypto component with the data
//! required for threshold signing and threshold signature verification.
//!
//! The `ThresholdSigner`'s sign method computes an individual threshold
//! signature share.
//!
//! The `ThresholdSigVerifier`'s methods perform the following operations on
//! signatures:
//! * Verify an individual signature share using `verify_threshold_sig_share`.
//! * Combine individual signature shares into a combined threshold signature
//!   using `combine_threshold_sig_shares`.
//! * Verify a combined threshold signature using
//!   `verify_threshold_sig_combined`.
//!
//! Please refer to the trait documentation for details.

use crate::crypto::hash::{
    DOMAIN_BLOCK, DOMAIN_CATCH_UP_CONTENT, DOMAIN_CERTIFICATION_CONTENT,
    DOMAIN_CRYPTO_HASH_OF_CANISTER_HTTP_RESPONSE_METADATA, DOMAIN_DEALING_CONTENT,
    DOMAIN_ECDSA_COMPLAINT_CONTENT, DOMAIN_ECDSA_OPENING_CONTENT, DOMAIN_FINALIZATION_CONTENT,
    DOMAIN_IDKG_DEALING, DOMAIN_NOTARIZATION_CONTENT, DOMAIN_RANDOM_BEACON_CONTENT,
    DOMAIN_RANDOM_TAPE_CONTENT, DOMAIN_SIGNED_IDKG_DEALING,
};
use ic_types::canister_http::CanisterHttpResponseMetadata;
use ic_types::crypto::canister_threshold_sig::idkg::{IDkgDealing, SignedIDkgDealing};
use ic_types::crypto::{
    BasicSigOf, CanisterSigOf, CombinedMultiSigOf, CryptoResult, IndividualMultiSigOf,
    SignedBytesWithoutDomainSeparator, UserPublicKey,
};
use ic_types::messages::{Delegation, MessageId, WebAuthnEnvelope};
use ic_types::{
    consensus::{
        certification::CertificationContent,
        dkg::DealingContent,
        ecdsa::{EcdsaComplaintContent, EcdsaOpeningContent, EcdsaSigShare},
        Block, CatchUpContent, CatchUpContentProtobufBytes, FinalizationContent,
        NotarizationContent, RandomBeaconContent, RandomTapeContent,
    },
    NodeId, RegistryVersion,
};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryFrom;

pub mod threshold_sig;

pub use threshold_sig::{ThresholdSigVerifier, ThresholdSigVerifierByPublicKey, ThresholdSigner};

pub mod canister_threshold_sig;

const SIG_DOMAIN_IC_REQUEST_AUTH_DELEGATION: &str = "ic-request-auth-delegation";
const SIG_DOMAIN_IC_REQUEST: &str = "ic-request";

/// `Signable` represents an object whose byte-vector representation
/// can be signed using a digital signature scheme.
/// It supports domain separation via `SignatureDomain` trait.
pub trait Signable: SignatureDomain + SignedBytesWithoutDomainSeparator {
    /// Returns a byte-vector that is used as input for signing/verification
    /// in a digital signature scheme.
    fn as_signed_bytes(&self) -> Vec<u8>;
}

impl<T> Signable for T
where
    T: SignatureDomain + SignedBytesWithoutDomainSeparator,
{
    fn as_signed_bytes(&self) -> Vec<u8> {
        let mut bytes = self.domain();
        bytes.append(&mut self.as_signed_bytes_without_domain_separator());
        bytes
    }
}

/// This trait is sealed and can only be implemented by types that are
/// explicitly approved by the Github owners of this file (that is, the
/// crypto team) via an implementation of the `SignatureDomainSeal`. Explicit
/// approval is required for security reasons to ensure proper domain
/// separation.
pub trait SignatureDomain: private::SignatureDomainSeal {
    fn domain(&self) -> Vec<u8>;
}

mod private {
    use super::*;
    use ic_types::crypto::canister_threshold_sig::idkg::{IDkgDealing, SignedIDkgDealing};

    pub trait SignatureDomainSeal {}

    impl SignatureDomainSeal for Block {}
    impl SignatureDomainSeal for DealingContent {}
    impl SignatureDomainSeal for NotarizationContent {}
    impl SignatureDomainSeal for FinalizationContent {}
    impl SignatureDomainSeal for IDkgDealing {}
    impl SignatureDomainSeal for SignedIDkgDealing {}
    impl SignatureDomainSeal for EcdsaSigShare {}
    impl SignatureDomainSeal for EcdsaComplaintContent {}
    impl SignatureDomainSeal for EcdsaOpeningContent {}
    impl SignatureDomainSeal for WebAuthnEnvelope {}
    impl SignatureDomainSeal for Delegation {}
    impl SignatureDomainSeal for CanisterHttpResponseMetadata {}
    impl SignatureDomainSeal for MessageId {}
    impl SignatureDomainSeal for CertificationContent {}
    impl SignatureDomainSeal for CatchUpContent {}
    impl SignatureDomainSeal for CatchUpContentProtobufBytes {}
    impl SignatureDomainSeal for RandomBeaconContent {}
    impl SignatureDomainSeal for RandomTapeContent {}
    impl SignatureDomainSeal for SignableMock {}
}

impl SignatureDomain for CanisterHttpResponseMetadata {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_CRYPTO_HASH_OF_CANISTER_HTTP_RESPONSE_METADATA)
    }
}

impl SignatureDomain for Block {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_BLOCK)
    }
}

impl SignatureDomain for DealingContent {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_DEALING_CONTENT)
    }
}

impl SignatureDomain for NotarizationContent {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_NOTARIZATION_CONTENT)
    }
}

impl SignatureDomain for FinalizationContent {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_FINALIZATION_CONTENT)
    }
}

impl SignatureDomain for IDkgDealing {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_IDKG_DEALING)
    }
}

impl SignatureDomain for SignedIDkgDealing {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_SIGNED_IDKG_DEALING)
    }
}

impl SignatureDomain for EcdsaSigShare {
    // ECDSA is an external standard, hence no domain is used.
    fn domain(&self) -> Vec<u8> {
        vec![]
    }
}

impl SignatureDomain for EcdsaComplaintContent {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_ECDSA_COMPLAINT_CONTENT)
    }
}

impl SignatureDomain for EcdsaOpeningContent {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_ECDSA_OPENING_CONTENT)
    }
}

impl SignatureDomain for WebAuthnEnvelope {
    // WebAuthn is an external standard, hence no domain is used.
    fn domain(&self) -> Vec<u8> {
        vec![]
    }
}

impl SignatureDomain for Delegation {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(SIG_DOMAIN_IC_REQUEST_AUTH_DELEGATION)
    }
}

impl SignatureDomain for MessageId {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(SIG_DOMAIN_IC_REQUEST)
    }
}

impl SignatureDomain for CertificationContent {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_CERTIFICATION_CONTENT)
    }
}

impl SignatureDomain for CatchUpContent {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_CATCH_UP_CONTENT)
    }
}

// This is INTENTIONALLY made the same as CatchUpContent, because this type is
// used to verify the signature over the bytes of a catch up package without
// necessarily needing to deserialize them into CatchUpContent.
impl SignatureDomain for CatchUpContentProtobufBytes {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_CATCH_UP_CONTENT)
    }
}

impl SignatureDomain for RandomBeaconContent {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_RANDOM_BEACON_CONTENT)
    }
}

impl SignatureDomain for RandomTapeContent {
    fn domain(&self) -> Vec<u8> {
        domain_with_prepended_length(DOMAIN_RANDOM_TAPE_CONTENT)
    }
}

// Returns a vector of bytes that contains the given domain
// prepended with a single byte that holds the length of the domain.
// This is the recommended format for non-empty domain separators,
// and this helper be used for simple implementations of
// `SignatureDomain`-trait, e.g.:
//
// const SOME_DOMAIN : &str = "some_domain";
//
// impl SignatureDomain for SomeDomain {
//     fn domain(&self) -> Vec<u8> {
//         domain_with_prepended_length(SOME_DOMAIN)
//     }
// }
fn domain_with_prepended_length(domain: &str) -> Vec<u8> {
    let domain_len = u8::try_from(domain.len()).expect("domain too long");
    let mut ret = vec![domain_len];
    ret.extend(domain.as_bytes());
    ret
}

/// A helper struct for testing that implements `Signable`.
///
/// `SignableMock` is needed for testing interfaces that use `Signable`-trait.
/// It is defined here because `SignatureDomain` is _sealed_ and must only be
/// implemented here in this crate.
///
/// Ideally, this struct would be annotated with `#[cfg(test)]` so that it is
/// only available in test code, however, then it would not be visible outside
/// of this crate where it is needed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SignableMock {
    pub domain: Vec<u8>,
    pub signed_bytes_without_domain: Vec<u8>,
}

impl SignableMock {
    pub fn new(signed_bytes_without_domain: Vec<u8>) -> Self {
        Self {
            domain: domain_with_prepended_length("signable_mock_domain"),
            signed_bytes_without_domain,
        }
    }
}

impl SignatureDomain for SignableMock {
    fn domain(&self) -> Vec<u8> {
        self.domain.clone()
    }
}

impl SignedBytesWithoutDomainSeparator for SignableMock {
    fn as_signed_bytes_without_domain_separator(&self) -> Vec<u8> {
        self.signed_bytes_without_domain.clone()
    }
}

/// A Crypto Component interface to create basic signatures.
///
/// Although the exact underlying signature scheme is unspecified and
/// potentially subject to change, it is guaranteed to be non-malleable,
/// that is, strongly unforgeable under chosen-message attack.
pub trait BasicSigner<T: Signable> {
    /// Creates a (non-malleable) basic signature.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if the `signer`'s public key cannot
    ///   be found at the given `registry_version`.
    /// * `CryptoError::MalformedPublicKey`: if the `signer`'s public key
    ///   obtained from the registry is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the `signer`'s public key
    ///   obtained from the registry is for an unsupported algorithm.
    /// * `CryptoError::SecretKeyNotFound`: if the `signer`'s secret key cannot
    ///   be found in the secret key store.
    /// * `CryptoError::MalformedSecretKey`: if the secret key is malformed.
    /// * `CryptoError::InvalidArgument`: if the signature algorithm is not
    ///   supported.
    fn sign_basic(
        &self,
        message: &T,
        signer: NodeId,
        registry_version: RegistryVersion,
    ) -> CryptoResult<BasicSigOf<T>>;
}

/// A Crypto Component interface to verify basic signatures.
pub trait BasicSigVerifier<T: Signable> {
    /// Verifies a basic signature.
    ///
    /// Although the exact underlying signature scheme is unspecified and
    /// potentially subject to change, it is guaranteed to be non-malleable,
    /// that is, strongly unforgeable under chosen-message attack.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if the `signer`'s public key cannot
    ///   be found at the given `registry_version`.
    /// * `CryptoError::MalformedSignature`: if the signature is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the signature algorithm is
    ///   not supported, or if the `signer`'s public key obtained from the
    ///   registry is for an unsupported algorithm.
    /// * `CryptoError::MalformedPublicKey`: if the `signer`'s public key
    ///   obtained from the registry is malformed.
    /// * `CryptoError::SignatureVerification`: if the `signature` could not be
    ///   verified.
    fn verify_basic_sig(
        &self,
        signature: &BasicSigOf<T>,
        message: &T,
        signer: NodeId,
        registry_version: RegistryVersion,
    ) -> CryptoResult<()>;
}

/// A Crypto Component interface to verify basic signatures by public key.
pub trait BasicSigVerifierByPublicKey<T: Signable> {
    /// Verifies a basic signature using the given `public_key`.
    ///
    /// # Errors
    /// * `CryptoError::MalformedPublicKey`: if the `public_key` is malformed.
    /// * `CryptoError::MalformedSignature`: if the `signature` is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the signature algorithm is
    ///   not supported, or if the `public_key` is for an unsupported algorithm.
    /// * `CryptoError::SignatureVerification`: if the `signature` could not be
    ///   verified.
    fn verify_basic_sig_by_public_key(
        &self,
        signature: &BasicSigOf<T>,
        signed_bytes: &T,
        public_key: &UserPublicKey,
    ) -> CryptoResult<()>;
}

/// A Crypto Component interface to verify (ICCSA) canister signatures.
pub trait CanisterSigVerifier<T: Signable> {
    /// Verifies an ICCSA canister signature.
    ///
    /// # Errors
    /// * `CryptoError::AlgorithmNotSupported`: if the signature algorithm is
    ///   not supported for canister signatures.
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::RootSubnetPublicKeyNotFound`: if the root subnet id or
    ///   the root subnet threshold signing public key cannot be found in the
    ///   registry at `registry_version`.
    /// * `CryptoError::MalformedPublicKey`: if the root subnet's threshold
    ///   signing public key is malformed.
    /// * `CryptoError::MalformedSignature`: if the `signature` is malformed.
    /// * `CryptoError::SignatureVerification`: if the `signature` could not be
    ///   verified.
    fn verify_canister_sig(
        &self,
        signature: &CanisterSigOf<T>,
        signed_bytes: &T,
        public_key: &UserPublicKey,
        registry_version: RegistryVersion,
    ) -> CryptoResult<()>;
}

/// A Crypto Component interface to verify ingress messages.
pub trait IngressSigVerifier:
    BasicSigVerifierByPublicKey<WebAuthnEnvelope>
    + BasicSigVerifierByPublicKey<MessageId>
    + BasicSigVerifierByPublicKey<Delegation>
    + CanisterSigVerifier<Delegation>
    + CanisterSigVerifier<MessageId>
{
}

impl<T> IngressSigVerifier for T where
    T: BasicSigVerifierByPublicKey<WebAuthnEnvelope>
        + BasicSigVerifierByPublicKey<MessageId>
        + BasicSigVerifierByPublicKey<Delegation>
        + CanisterSigVerifier<Delegation>
        + CanisterSigVerifier<MessageId>
{
}

/// A Crypto Component interface to create multi-signatures.
pub trait MultiSigner<T: Signable> {
    /// Creates an individual multi-signature.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if the public key cannot be found at
    ///   the given `registry_version`.
    /// * `CryptoError::MalformedPublicKey`: if the public key obtained from the
    ///   registry is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the public key obtained from
    ///   the registry is for an unsupported algorithm.
    /// * `CryptoError::SecretKeyNotFound`: if the signing key cannot be found
    ///   in the secret key store.
    /// * `CryptoError::MalformedSecretKey`: if the secret key is malformed.
    fn sign_multi(
        &self,
        message: &T,
        signer: NodeId,
        registry_version: RegistryVersion,
    ) -> CryptoResult<IndividualMultiSigOf<T>>;
}

/// A Crypto Component interface to verify and combine multi-signatures.
pub trait MultiSigVerifier<T: Signable> {
    /// Verifies an individual multi-signature.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if the public key cannot be found at
    ///   the given `registry_version`.
    /// * `CryptoError::MalformedSignature`: if the mutli-signature is
    ///   malformed.
    /// * `CryptoError::MalformedPublicKey`: if the public key obtained from the
    ///   registry is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the public key obtained from
    ///   the registry is for an unsupported algorithm.
    /// * `CryptoError::SignatureVerification`: if the individual
    ///   multi-signature could not be verified.
    fn verify_multi_sig_individual(
        &self,
        signature: &IndividualMultiSigOf<T>,
        message: &T,
        signer: NodeId,
        registry_version: RegistryVersion,
    ) -> CryptoResult<()>;

    /// Combines individual multi-signature shares.
    ///
    /// The registry version is not needed for the cryptographic scheme we use
    /// currently/initially. Yet it is a parameter so that we can switch to
    /// other schemes without affecting the API.
    ///
    /// Note that the resulting combined signature will only be valid if all the
    /// individual signatures are valid, i.e. `verify_multi_sig_individual`
    /// returned `Ok`.
    ///
    /// the individual multi-signatures passed as `signatures` must have been
    ///   verified using `verify_multi_sig_individual`.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if any of the public keys for the
    ///   signatures cannot be found at the given `registry_version`.
    /// * `CryptoError::MalformedSignature`: if any of the mutli-signatures is
    ///   malformed.
    /// * `CryptoError::MalformedPublicKey`: if any of the public keys obtained
    ///   from the registry is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if any of the public keys
    ///   obtained from the registry or a signature is for an unsupported
    ///   algorithm.
    ///
    /// # Panics
    /// * if `signatures` is empty.
    fn combine_multi_sig_individuals(
        &self,
        signatures: BTreeMap<NodeId, IndividualMultiSigOf<T>>,
        registry_version: RegistryVersion,
    ) -> CryptoResult<CombinedMultiSigOf<T>>;

    /// Verifies a combined multi-signature.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if any of the public keys for the
    ///   'signers' cannot be found at the given `registry_version`.
    /// * `CryptoError::MalformedPublicKey`: if any of the public keys obtained
    ///   from the registry is malformed.
    /// * `CryptoError::MalformedSignature`: if the combined `signature` is
    ///   malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if any of the public keys
    ///   obtained from the registry or the combined signature is for an
    ///   unsupported algorithm. obtained from the registry or the combined
    ///   signature is for an unsupported algorithm.
    /// * `CryptoError::SignatureVerification`: if the combined multi-signature
    ///   could not be verified.
    ///
    /// # Panics
    /// * if `signers` are empty.
    fn verify_multi_sig_combined(
        &self,
        signature: &CombinedMultiSigOf<T>,
        message: &T,
        signers: BTreeSet<NodeId>,
        registry_version: RegistryVersion,
    ) -> CryptoResult<()>;
}
