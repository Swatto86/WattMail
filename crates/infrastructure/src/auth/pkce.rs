//! PKCE (RFC 7636, S256) and random URL-safe token generation.

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// A PKCE verifier/challenge pair.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh pair from 32 bytes of OS randomness.
    pub fn generate() -> Self {
        let verifier = random_token();
        let challenge = b64url(Sha256::digest(verifier.as_bytes()).as_slice());
        Self {
            verifier,
            challenge,
        }
    }
}

/// A random URL-safe token (43 chars), used for PKCE verifiers and CSRF state.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    b64url(&bytes)
}

fn b64url(input: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
}
