//! Keys, content ids and authenticated encryption.
//!
//! - A repository has random master keys: a 32-byte encryption key, a 32-byte id key and a
//!   chunker seed. They are stored in `keys/<name>` encrypted with a key derived from the
//!   password by Argon2id.
//! - Blob ids are BLAKE3 keyed hashes of the plaintext, so ids reveal nothing about content to
//!   someone without the keys, and a decrypted blob is checked against its id.
//! - Everything stored is sealed with XChaCha20-Poly1305 (random 192-bit nonces):
//!   `nonce (24) || ciphertext || tag (16)`. Inside, one flag byte says whether the payload is
//!   zstd-compressed.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

pub type Id = [u8; 32];

#[derive(Debug)]
pub struct CryptoError(pub String);

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CryptoError {}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("operating system random number generator");
    b
}

#[derive(Clone)]
pub struct Keys {
    pub enc: [u8; 32],
    pub id: [u8; 32],
    pub chunker_seed: u64,
}

#[derive(Serialize, Deserialize)]
struct KeysPlain {
    enc: String,
    id: String,
    chunker_seed: u64,
}

/// A key file: KDF parameters and the sealed master keys.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KeyFile {
    pub kdf: String,
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
    pub salt: String,
    pub sealed: String,
}

fn kek(password: &[u8], salt: &[u8], m: u32, t: u32, p: u32) -> Result<[u8; 32], CryptoError> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| CryptoError(e.to_string()))?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password, salt, &mut out)
        .map_err(|e| CryptoError(e.to_string()))?;
    Ok(out)
}

impl Keys {
    pub fn generate() -> Keys {
        Keys { enc: random_bytes(), id: random_bytes(), chunker_seed: u64::from_le_bytes(random_bytes()) }
    }

    pub fn id_of(&self, data: &[u8]) -> Id {
        *blake3::keyed_hash(&self.id, data).as_bytes()
    }

    /// Seals `data` (compressing it first when that helps).
    pub fn seal(&self, data: &[u8], compress: bool) -> Vec<u8> {
        let mut plain = Vec::with_capacity(data.len() + 1);
        let packed = if compress { zstd::bulk::compress(data, 3).ok().filter(|c| c.len() + 16 < data.len()) } else { None };
        match packed {
            Some(c) => {
                plain.push(1);
                plain.extend_from_slice(&c);
            }
            None => {
                plain.push(0);
                plain.extend_from_slice(data);
            }
        }
        seal_raw(&self.enc, &plain)
    }

    /// Opens a sealed object; fails if it was modified in any way.
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let plain = open_raw(&self.enc, sealed)?;
        match plain.split_first() {
            Some((0, rest)) => Ok(rest.to_vec()),
            Some((1, rest)) => zstd::stream::decode_all(rest).map_err(|e| CryptoError(format!("decompress: {e}"))),
            _ => Err(CryptoError("bad payload flag".into())),
        }
    }

    pub fn to_keyfile(&self, password: &[u8], m_kib: u32, t: u32) -> Result<KeyFile, CryptoError> {
        let salt: [u8; 16] = random_bytes();
        let k = kek(password, &salt, m_kib, t, 1)?;
        let plain = serde_json::to_vec(&KeysPlain { enc: hex::encode(self.enc), id: hex::encode(self.id), chunker_seed: self.chunker_seed }).unwrap();
        Ok(KeyFile { kdf: "argon2id".into(), m_kib, t, p: 1, salt: hex::encode(salt), sealed: hex::encode(seal_raw(&k, &plain)) })
    }

    pub fn from_keyfile(kf: &KeyFile, password: &[u8]) -> Result<Keys, CryptoError> {
        let salt = hex::decode(&kf.salt).map_err(|e| CryptoError(e.to_string()))?;
        let k = kek(password, &salt, kf.m_kib, kf.t, kf.p)?;
        let sealed = hex::decode(&kf.sealed).map_err(|e| CryptoError(e.to_string()))?;
        let plain = open_raw(&k, &sealed).map_err(|_| CryptoError("wrong password or damaged key file".into()))?;
        let p: KeysPlain = serde_json::from_slice(&plain).map_err(|e| CryptoError(e.to_string()))?;
        let arr = |s: &str| -> Result<[u8; 32], CryptoError> {
            hex::decode(s).ok().and_then(|v| v.try_into().ok()).ok_or_else(|| CryptoError("bad key".into()))
        };
        Ok(Keys { enc: arr(&p.enc)?, id: arr(&p.id)?, chunker_seed: p.chunker_seed })
    }
}

fn seal_raw(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
    let nonce: [u8; 24] = random_bytes();
    let c = XChaCha20Poly1305::new(key.into());
    let ct = c.encrypt(XNonce::from_slice(&nonce), plain).expect("encryption");
    let mut out = Vec::with_capacity(24 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

fn open_raw(key: &[u8; 32], sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if sealed.len() < 24 + 16 {
        return Err(CryptoError("object too short".into()));
    }
    let c = XChaCha20Poly1305::new(key.into());
    c.decrypt(XNonce::from_slice(&sealed[..24]), &sealed[24..]).map_err(|_| CryptoError("authentication failed (object modified or wrong key)".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_tamper() {
        let k = Keys::generate();
        let data = b"hello hello hello hello hello hello hello hello hello hello".repeat(20);
        let s = k.seal(&data, true);
        assert!(s.len() < data.len()); // compressed
        assert_eq!(k.open(&s).unwrap(), data);
        for i in [0, 30, s.len() - 1] {
            let mut bad = s.clone();
            bad[i] ^= 1;
            assert!(k.open(&bad).is_err(), "flip at {i} not detected");
        }
        assert!(Keys::generate().open(&s).is_err());
        let r: Vec<u8> = (0..1000).map(|_| fastrand::u8(..)).collect();
        assert_eq!(k.open(&k.seal(&r, true)).unwrap(), r); // incompressible: stored raw
    }

    #[test]
    fn key_files() {
        let k = Keys::generate();
        let kf = k.to_keyfile(b"correct horse", 64, 1).unwrap();
        let back = Keys::from_keyfile(&kf, b"correct horse").unwrap();
        assert_eq!(back.enc, k.enc);
        assert_eq!(back.chunker_seed, k.chunker_seed);
        assert!(Keys::from_keyfile(&kf, b"wrong").is_err());
        assert_ne!(k.id_of(b"x"), Keys::generate().id_of(b"x"));
    }
}
