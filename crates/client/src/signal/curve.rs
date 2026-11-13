//
// Copyright 2020-2025 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//
// From libsignal v0.74.1, rust/core/src/curve.rs and curve/curve25519.rs. Modified for
// whatsapp-sync: only Curve25519 ("DJB") keys, verification without signing, and two later
// upstream hardening fixes applied: Diffie-Hellman agreements that come out all-zero are
// refused (libsignal dce9c0d30, 2026-02), and `is_canonical` (libsignal 5293caa6c, 2025-09).

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::montgomery::MontgomeryPoint;
use curve25519_dalek::scalar::Scalar;
use sha2::{Digest, Sha512};
use subtle::ConstantTimeEq;
use x25519_dalek::StaticSecret;

/// libsignal's type byte for a Curve25519 public key.
const DJB_TYPE: u8 = 0x05;
const SIGNATURE_LENGTH: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("public key is not 0x05 followed by 32 bytes")]
    BadPublicKey,
    #[error("private key is not 32 bytes")]
    BadPrivateKey,
    #[error("key agreement produced the all-zero secret")]
    InvalidAgreement,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey([u8; 32]);

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublicKey(..)")
    }
}

impl PublicKey {
    /// The bare 32-byte form.
    pub fn from_djb(bytes: &[u8]) -> Result<Self, KeyError> {
        bytes
            .try_into()
            .map(Self)
            .map_err(|_| KeyError::BadPublicKey)
    }

    /// The 33-byte form with libsignal's type byte, as it travels in messages.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, KeyError> {
        match bytes.split_first() {
            Some((&DJB_TYPE, key)) => Self::from_djb(key),
            _ => Err(KeyError::BadPublicKey),
        }
    }

    pub fn serialize(&self) -> [u8; 33] {
        let mut out = [DJB_TYPE; 33];
        out[1..].copy_from_slice(&self.0);
        out
    }

    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// XEdDSA: a Curve25519 key verifying an Ed25519-style signature, the sign of the Edwards
    /// point riding in the signature's top bit.
    pub fn verify_signature(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(signature) = <&[u8; SIGNATURE_LENGTH]>::try_from(signature) else {
            return false;
        };
        let sign = (signature[SIGNATURE_LENGTH - 1] & 0b1000_0000) >> 7;
        let Some(ed_public) = MontgomeryPoint(self.0).to_edwards(sign) else {
            return false;
        };
        let cap_a = ed_public.compress();
        let mut cap_r = [0u8; 32];
        cap_r.copy_from_slice(&signature[..32]);
        let mut s = [0u8; 32];
        s.copy_from_slice(&signature[32..]);
        s[31] &= 0b0111_1111;
        if s[31] & 0b1110_0000 != 0 {
            return false;
        }
        let mut hash = Sha512::new();
        hash.update(cap_r);
        hash.update(cap_a.as_bytes());
        hash.update(message);
        let h = Scalar::from_hash(hash);
        let check = EdwardsPoint::vartime_double_scalar_mul_basepoint(
            &h,
            &-ed_public,
            &Scalar::from_bytes_mod_order(s),
        )
        .compress();
        bool::from(check.as_bytes().ct_eq(&cap_r))
    }

    /// Whether this is a key an honest party could have made: no small-order component, and
    /// encoded below the field prime. A session must not start from any other base key.
    pub fn is_canonical(&self) -> bool {
        self.is_torsion_free() && self.is_in_range()
    }

    fn is_torsion_free(&self) -> bool {
        MontgomeryPoint(self.0)
            .to_edwards(0)
            .is_some_and(|point| point.is_torsion_free())
    }

    fn is_in_range(&self) -> bool {
        let k = &self.0;
        // Not above 2^255 - 19: neither the high bit set, nor 2^255 - 19 .. 2^255 - 1.
        !(k[31] & 0b1000_0000 != 0
            || (k[0] >= 0u8.wrapping_sub(19) && k[1..31] == [0xFF; 30] && k[31] == 0x7F))
    }
}

#[derive(Clone)]
pub struct PrivateKey(StaticSecret);

impl PrivateKey {
    pub fn deserialize(bytes: &[u8]) -> Result<Self, KeyError> {
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| KeyError::BadPrivateKey)?;
        Ok(Self(StaticSecret::from(bytes)))
    }

    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
        Self(StaticSecret::from(bytes))
    }

    pub fn serialize(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey(*x25519_dalek::PublicKey::from(&self.0).as_bytes())
    }

    /// X25519, refusing the all-zero result a small-order public key forces (RFC 7748 §6.1).
    pub fn agreement(&self, theirs: &PublicKey) -> Result<[u8; 32], KeyError> {
        let shared = *self
            .0
            .diffie_hellman(&x25519_dalek::PublicKey::from(theirs.0))
            .as_bytes();
        if bool::from(shared.ct_eq(&[0u8; 32])) {
            return Err(KeyError::InvalidAgreement);
        }
        Ok(shared)
    }
}

#[derive(Clone)]
pub struct KeyPair {
    pub public: PublicKey,
    pub private: PrivateKey,
}

impl KeyPair {
    pub fn generate() -> Self {
        let private = PrivateKey::generate();
        Self {
            public: private.public_key(),
            private,
        }
    }
}

#[cfg(test)]
mod tests {
    use curve25519_dalek::constants::EIGHT_TORSION;

    use super::*;

    #[test]
    fn honest_keys_are_canonical() {
        assert!(KeyPair::generate().public.is_canonical());
    }

    #[test]
    fn keys_with_a_torsion_component_are_not() {
        let key = KeyPair::generate().public;
        let point = MontgomeryPoint(key.0).to_edwards(0).unwrap();
        for torsion in EIGHT_TORSION.iter().skip(1) {
            let tweaked = PublicKey((point + torsion).to_montgomery().to_bytes());
            assert!(!tweaked.is_canonical());
        }
    }

    #[test]
    fn keys_at_or_above_the_prime_are_not() {
        let mut high_bit = KeyPair::generate().public.0;
        high_bit[31] |= 0x80;
        assert!(!PublicKey(high_bit).is_in_range());
        let mut prime = [0xFF; 32];
        prime[0] = 0xED;
        prime[31] = 0x7F;
        assert!(!PublicKey(prime).is_in_range());
        prime[0] = 0xEC;
        assert!(PublicKey(prime).is_in_range());
    }

    #[test]
    fn an_agreement_with_a_small_order_key_is_refused() {
        let ours = PrivateKey::generate();
        assert_eq!(
            ours.agreement(&PublicKey([0; 32])),
            Err(KeyError::InvalidAgreement)
        );
        assert!(ours.agreement(&KeyPair::generate().public).is_ok());
    }
}
