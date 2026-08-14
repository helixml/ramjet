//! Shared privacy boundary for caller-provided session identifiers.

use std::fmt;

use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum OpaqueSession<'a> {
    Missing,
    Invalid,
    Valid(&'a [u8]),
}

impl fmt::Debug for OpaqueSession<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "OpaqueSession::Missing",
            Self::Invalid => "OpaqueSession::Invalid",
            Self::Valid(_) => "OpaqueSession::Valid(<redacted>)",
        })
    }
}

pub(crate) fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut normalized = [0_u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; BLOCK_BYTES];
    for ((inner, outer), key_byte) in inner_pad.iter_mut().zip(&mut outer_pad).zip(normalized) {
        *inner ^= key_byte;
        *outer ^= key_byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for part in parts {
        inner.update(part);
    }
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_session_debug_is_content_free() {
        let rendered = format!("{:?}", OpaqueSession::Valid(b"private-customer-session"));
        assert_eq!(rendered, "OpaqueSession::Valid(<redacted>)");
        assert!(!rendered.contains("private"));
    }

    #[test]
    fn hmac_matches_rfc_4231_sha256_case_one() {
        assert_eq!(
            hmac_sha256(&[0x0b; 20], &[b"Hi There"]),
            [
                0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
                0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
                0x2e, 0x32, 0xcf, 0xf7,
            ]
        );
    }
}
