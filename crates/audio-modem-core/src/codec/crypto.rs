//! Argon2id key derivation and AES-256-GCM authenticated encryption.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::CryptoError;

/// Argon2id salt length.
pub const SALT_LEN: usize = 16;
/// AES-GCM nonce length. 96 bits is the only size with a standardised,
/// collision-free counter construction; other lengths are hashed through
/// GHASH first and buy nothing.
pub const NONCE_LEN: usize = 12;
/// AES-GCM authentication tag length.
pub const TAG_LEN: usize = 16;
/// AES-256 key length.
pub const KEY_LEN: usize = 32;

/// Argon2id cost parameters.
///
/// These are written into the frame header rather than hardcoded at both ends.
/// Baking them into the binary would mean any future retuning silently breaks
/// every previously encoded file — the decoder must be told what the encoder
/// used, not assume it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory cost in KiB.
    pub m_cost: u32,
    /// Iteration count.
    pub t_cost: u32,
    /// Degree of parallelism.
    pub p_cost: u32,
}

impl Default for KdfParams {
    /// 64 MiB, 3 passes, 1 lane.
    ///
    /// Argon2id's resistance is bought almost entirely with *memory*, not
    /// iterations: 64 MiB is what makes a GPU or ASIC attacker's parallelism
    /// collapse, because each guess needs its own 64 MiB working set and
    /// memory bandwidth becomes the bottleneck rather than raw arithmetic.
    /// `p_cost = 1` keeps derivation single-threaded and reproducible; raising
    /// it would need the `argon2/parallel` feature to actually buy wall-clock
    /// time, and it does not change the memory-hardness argument.
    fn default() -> Self {
        Self {
            m_cost: 65_536,
            t_cost: 3,
            p_cost: 1,
        }
    }
}

/// A derived AES-256 key that scrubs itself on drop.
///
/// `ZeroizeOnDrop` is not a guarantee against a determined attacker with
/// memory access — the key still passes through registers and possibly a swap
/// file, and Rust may have moved the value before this runs. It shortens the
/// window; it does not close it.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct DerivedKey([u8; KEY_LEN]);

impl DerivedKey {
    /// Borrow the raw key bytes.
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for DerivedKey {
    /// Never print key material, not even in a panic message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DerivedKey(<redacted>)")
    }
}

/// Fill `dest` from the operating system's CSPRNG.
///
/// Every salt and nonce comes from here. Note that GCM nonces are generated
/// randomly rather than by counter: at 96 bits, the birthday bound puts a
/// meaningful collision risk somewhere past 2^32 messages under one key, and
/// each file here derives a fresh key from a fresh salt anyway, so a repeat
/// would need both to collide.
pub fn fill_random(dest: &mut [u8]) -> Result<(), CryptoError> {
    getrandom::fill(dest).map_err(|error| CryptoError::Rng(error.to_string()))
}

/// Derive an AES-256 key from `passphrase` and `salt` using Argon2id.
///
/// Argon2id rather than Argon2i or Argon2d: it runs a data-independent first
/// pass (resisting side-channel attacks that watch memory access patterns) and
/// a data-dependent second pass (resisting time-memory tradeoffs), which is the
/// right default when the attacker model includes both a local observer and an
/// offline cracker.
pub fn derive_key(
    passphrase: &[u8],
    salt: &[u8; SALT_LEN],
    params: KdfParams,
) -> Result<DerivedKey, CryptoError> {
    let argon_params = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(KEY_LEN))
        .map_err(|error| CryptoError::KeyDerivation(error.to_string()))?;

    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);

    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase, salt, &mut key)
        .map_err(|error| CryptoError::KeyDerivation(error.to_string()))?;

    let derived = DerivedKey(key);
    key.zeroize();
    Ok(derived)
}

/// Encrypt `plaintext`, binding `aad` into the authentication tag.
///
/// Returns ciphertext with the 16-byte tag appended.
pub fn encrypt(
    key: &DerivedKey,
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_bytes()));
    cipher
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Encrypt)
}

/// Decrypt and verify `ciphertext` against `aad`.
///
/// The header fields passed as `aad` are authenticated but not encrypted, so
/// an attacker who edits a declared length or a KDF parameter in transit
/// invalidates the tag and lands here rather than steering the decoder.
pub fn decrypt(
    key: &DerivedKey,
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_bytes()));
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Decrypt)
}
