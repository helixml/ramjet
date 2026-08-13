//! Keyed, content-safe commitments for exact KV block indexing.
//!
//! A [`BlockDigester`] owns one 256-bit secret. The secret is deliberately not
//! serializable, cloneable, or exposed through `Debug`; snapshots carry only
//! [`KeyId`] and block commitments. Callers are responsible for loading the
//! secret from an appropriately protected source and for retaining it for the
//! lifetime of every index that uses its commitments.

use std::fmt;

use sha2::{Digest, Sha256};
use thiserror::Error;

const SHA256_BYTES: usize = 32;
const HMAC_BLOCK_BYTES: usize = 64;
const PRIMARY_BYTES: usize = 16;
const KEY_ID_DOMAIN: &[u8] = b"mini-dynamo:block-digest-key-id:v1\0";
const BLOCK_DOMAIN: &[u8] = b"mini-dynamo:block-digest:v1\0";

/// Public, deterministic identity of the secret used for block commitments.
///
/// This value may be serialized in a snapshot. It is not the secret itself.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct KeyId([u8; SHA256_BYTES]);

impl KeyId {
    #[must_use]
    pub(crate) const fn as_bytes(&self) -> &[u8; SHA256_BYTES] {
        &self.0
    }

    #[must_use]
    pub(crate) fn to_vec(self) -> Vec<u8> {
        self.0.to_vec()
    }

    /// Compare a wire key ID without returning at the first differing byte.
    ///
    /// A key ID is public metadata, so its length is not secret. All 32 content
    /// bytes are nevertheless inspected for an exact-length candidate.
    #[must_use]
    pub(crate) fn matches_wire(self, candidate: &[u8]) -> bool {
        if candidate.len() != SHA256_BYTES {
            return false;
        }
        let mut difference = 0_u8;
        for (expected, actual) in self.0.iter().zip(candidate) {
            difference |= expected ^ actual;
        }
        difference == 0
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("KeyId([REDACTED])")
    }
}

/// Compact radix-key portion of a block commitment.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PrimaryCommitment {
    pub(crate) token_count: u32,
    pub(crate) digest: [u8; PRIMARY_BYTES],
}

/// Full 256-bit keyed block commitment split into radix key and collision guard.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct BlockCommitment {
    pub(crate) primary: PrimaryCommitment,
    pub(crate) guard: [u8; SHA256_BYTES - PRIMARY_BYTES],
}

impl BlockCommitment {
    /// Reconstitute the complete HMAC stored in a snapshot record.
    #[must_use]
    pub(crate) fn digest_bytes(self) -> [u8; SHA256_BYTES] {
        let mut digest = [0_u8; SHA256_BYTES];
        digest[..PRIMARY_BYTES].copy_from_slice(&self.primary.digest);
        digest[PRIMARY_BYTES..].copy_from_slice(&self.guard);
        digest
    }
}

/// Static, content-free failures from block commitment construction.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum BlockDigestError {
    #[error("block digest secret must contain exactly 32 bytes")]
    InvalidSecretLength,
    #[error("KV block token count exceeds the supported range")]
    TokenCountOverflow,
}

/// HMAC-SHA-256 block commitment key.
///
/// The type intentionally implements neither `Clone` nor serialization. Its
/// `Debug` representation reveals only the public key identity, and `Drop`
/// overwrites the directly owned secret as a defense in depth measure.
pub(crate) struct BlockDigester {
    secret: [u8; SHA256_BYTES],
    key_id: KeyId,
}

impl BlockDigester {
    /// Take ownership of an exact-size secret.
    #[must_use]
    pub(crate) fn new(secret: [u8; SHA256_BYTES]) -> Self {
        let key_id = derive_key_id(&secret);
        Self { secret, key_id }
    }

    /// Copy an exact-size secret from protected configuration storage.
    pub(crate) fn from_slice(secret: &[u8]) -> Result<Self, BlockDigestError> {
        let secret = <[u8; SHA256_BYTES]>::try_from(secret)
            .map_err(|_| BlockDigestError::InvalidSecretLength)?;
        Ok(Self::new(secret))
    }

    #[must_use]
    pub(crate) const fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// Commit one logical KV block.
    ///
    /// The HMAC input is unambiguous: a versioned domain, a little-endian u32
    /// token count, then fixed-width little-endian token IDs. No raw token is
    /// retained in the returned value.
    pub(crate) fn commit(&self, token_ids: &[u32]) -> Result<BlockCommitment, BlockDigestError> {
        let token_count =
            u32::try_from(token_ids.len()).map_err(|_| BlockDigestError::TokenCountOverflow)?;
        let token_count_bytes = token_count.to_le_bytes();

        let mut inner = HmacSha256::new(&self.secret);
        inner.update(BLOCK_DOMAIN);
        inner.update(&token_count_bytes);
        for token_id in token_ids {
            inner.update(&token_id.to_le_bytes());
        }
        let digest = inner.finalize();

        let mut primary = [0_u8; PRIMARY_BYTES];
        let mut guard = [0_u8; SHA256_BYTES - PRIMARY_BYTES];
        primary.copy_from_slice(&digest[..PRIMARY_BYTES]);
        guard.copy_from_slice(&digest[PRIMARY_BYTES..]);
        Ok(BlockCommitment {
            primary: PrimaryCommitment {
                token_count,
                digest: primary,
            },
            guard,
        })
    }
}

impl fmt::Debug for BlockDigester {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlockDigester")
            .field("key_id", &self.key_id)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl Drop for BlockDigester {
    fn drop(&mut self) {
        // This avoids leaving the directly owned bytes intact. Stronger
        // compiler-guaranteed zeroization requires a dedicated direct
        // dependency and is intentionally not claimed here.
        self.secret.fill(0);
    }
}

fn derive_key_id(secret: &[u8; SHA256_BYTES]) -> KeyId {
    let mut hasher = Sha256::new();
    hasher.update(KEY_ID_DOMAIN);
    hasher.update(secret);
    KeyId(hasher.finalize().into())
}

/// Minimal streaming HMAC kept private so raw token buffers never need to be
/// assembled into a second allocation.
struct HmacSha256 {
    inner: Sha256,
    outer_pad: [u8; HMAC_BLOCK_BYTES],
}

impl HmacSha256 {
    fn new(key: &[u8]) -> Self {
        let mut normalized = [0_u8; HMAC_BLOCK_BYTES];
        if key.len() > HMAC_BLOCK_BYTES {
            normalized[..SHA256_BYTES].copy_from_slice(&Sha256::digest(key));
        } else {
            normalized[..key.len()].copy_from_slice(key);
        }
        let mut inner_pad = [0x36_u8; HMAC_BLOCK_BYTES];
        let mut outer_pad = [0x5c_u8; HMAC_BLOCK_BYTES];
        for ((inner, outer), key_byte) in inner_pad.iter_mut().zip(&mut outer_pad).zip(normalized) {
            *inner ^= key_byte;
            *outer ^= key_byte;
        }
        let mut inner = Sha256::new();
        inner.update(inner_pad);
        normalized.fill(0);
        inner_pad.fill(0);
        Self { inner, outer_pad }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    fn finalize(mut self) -> [u8; SHA256_BYTES] {
        let inner_digest = self.inner.finalize();
        let mut outer = Sha256::new();
        outer.update(self.outer_pad);
        outer.update(inner_digest);
        self.outer_pad.fill(0);
        outer.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; SHA256_BYTES] = *b"0123456789abcdef0123456789abcdef";

    #[test]
    fn hmac_matches_rfc_4231_vector() {
        let mut hmac = HmacSha256::new(&[0x0b; 20]);
        hmac.update(b"Hi There");
        assert_eq!(
            hmac.finalize(),
            [
                0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
                0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
                0x2e, 0x32, 0xcf, 0xf7,
            ]
        );
    }

    #[test]
    fn key_identity_is_deterministic_and_keyed() {
        let first = BlockDigester::new(SECRET);
        let second = BlockDigester::new(SECRET);
        let different = BlockDigester::new([0x5a; SHA256_BYTES]);
        assert_eq!(first.key_id(), second.key_id());
        assert_ne!(first.key_id(), different.key_id());
        assert!(first.key_id().matches_wire(first.key_id().as_bytes()));

        let mut changed = first.key_id().to_vec();
        changed[17] ^= 1;
        assert!(!first.key_id().matches_wire(&changed));
        assert!(!first.key_id().matches_wire(&changed[..31]));
        assert!(!first.key_id().matches_wire(&[0_u8; 33]));
    }

    #[test]
    fn commitment_is_deterministic_split_and_domain_separated() {
        let digester = BlockDigester::new(SECRET);
        let commitment = digester.commit(&[1, 2, 65_537, u32::MAX]).unwrap();
        assert_eq!(
            commitment,
            digester.commit(&[1, 2, 65_537, u32::MAX]).unwrap()
        );
        assert_eq!(commitment.primary.token_count, 4);
        assert_eq!(
            &commitment.digest_bytes()[..PRIMARY_BYTES],
            &commitment.primary.digest
        );
        assert_eq!(
            &commitment.digest_bytes()[PRIMARY_BYTES..],
            &commitment.guard
        );

        let mut undomained = HmacSha256::new(&SECRET);
        undomained.update(&4_u32.to_le_bytes());
        for token in [1_u32, 2, 65_537, u32::MAX] {
            undomained.update(&token.to_le_bytes());
        }
        assert_ne!(commitment.digest_bytes(), undomained.finalize());
    }

    #[test]
    fn key_id_and_commitment_wire_encoding_are_golden() {
        let digester = BlockDigester::new(SECRET);
        assert_eq!(
            digester.key_id().as_bytes(),
            &[
                0x9c, 0xda, 0xd9, 0x04, 0x1a, 0x32, 0x43, 0x21, 0x36, 0xe7, 0xcf, 0x00, 0x9f, 0x2f,
                0x95, 0xfe, 0x1b, 0x40, 0x9c, 0xca, 0xc6, 0xc4, 0x7d, 0x6d, 0x3a, 0x0d, 0x99, 0x1d,
                0x8d, 0x8a, 0x21, 0x40,
            ]
        );
        assert_eq!(
            digester
                .commit(&[1, 2, 65_537, u32::MAX])
                .unwrap()
                .digest_bytes(),
            [
                0xde, 0x77, 0x2a, 0x33, 0x84, 0x86, 0x6d, 0x41, 0x60, 0x04, 0x2d, 0x34, 0x56, 0x51,
                0x7b, 0xcd, 0xa4, 0xc6, 0x9c, 0xde, 0xf4, 0xac, 0x34, 0x24, 0x35, 0x13, 0x8d, 0xfd,
                0x14, 0xcd, 0xd7, 0xe3,
            ]
        );
    }

    #[test]
    fn boundaries_and_order_change_commitments() {
        let digester = BlockDigester::new(SECRET);
        assert_ne!(
            digester.commit(&[1, 2]).unwrap(),
            digester.commit(&[2, 1]).unwrap()
        );
        assert_ne!(
            digester.commit(&[1, 2]).unwrap(),
            digester.commit(&[1, 2, 0]).unwrap()
        );
        assert_ne!(
            digester.commit(&[]).unwrap(),
            digester.commit(&[0]).unwrap()
        );
        assert_ne!(
            digester.commit(&[0x0001_0002]).unwrap(),
            digester.commit(&[1, 2]).unwrap()
        );
    }

    #[test]
    fn secret_input_and_debug_are_content_safe() {
        assert_eq!(
            BlockDigester::from_slice(&SECRET[..31]).unwrap_err(),
            BlockDigestError::InvalidSecretLength
        );
        let digester = BlockDigester::from_slice(&SECRET).unwrap();
        let debug = format!("{digester:?}");
        assert!(!debug.contains("0123456789abcdef"));
        assert!(debug.contains("[REDACTED]"));

        let commitment = digester.commit(&[123_456_789]).unwrap();
        assert!(!format!("{commitment:?}").contains("123456789"));
    }
}
