use super::DecryptionError;
use super::pkcs5::Pkcs5;
use super::rc4::Rc4;
use crate::ObjectId;
use aes::cipher::block_padding::NoPadding;
use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use md5::{Digest as _, Md5};
use rand::RngExt as _;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// Per-object key material.  The longest key is AES-256; keeping the bytes
/// inline avoids two heap allocations for every encrypted string or stream.
#[derive(Clone, Copy, Debug)]
pub struct ObjectKey {
    bytes: [u8; 32],
    len: u8,
}

impl ObjectKey {
    fn new(bytes: &[u8]) -> Self {
        let len = bytes.len().min(32);
        let mut out = [0u8; 32];
        out[..len].copy_from_slice(&bytes[..len]);
        Self {
            bytes: out,
            len: len as u8,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

impl AsRef<[u8]> for ObjectKey {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

pub trait CryptFilter: std::fmt::Debug + Send + Sync {
    fn method(&self) -> &[u8];
    fn compute_key(&self, key: &[u8], obj_id: ObjectId) -> Result<ObjectKey, DecryptionError>;
    fn encrypt(&self, key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, DecryptionError>;
    fn decrypt(&self, key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, DecryptionError>;
}

#[derive(Clone, Copy, Debug)]
pub struct IdentityCryptFilter;

impl CryptFilter for IdentityCryptFilter {
    fn method(&self) -> &[u8] {
        b"Identity"
    }

    fn compute_key(&self, key: &[u8], _obj_id: ObjectId) -> Result<ObjectKey, DecryptionError> {
        Ok(ObjectKey::new(key))
    }

    fn encrypt(&self, _key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, DecryptionError> {
        Ok(plaintext.to_vec())
    }

    fn decrypt(&self, _key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, DecryptionError> {
        Ok(ciphertext.to_vec())
    }
}

/// Extend the file key with the low-order 3 bytes of the object number and the
/// low-order 2 bytes of the generation number, hash it, and keep the first
/// min(n + 5, 16) bytes.  `salt` adds the AES "sAlT" marker (Algorithm 1).
fn md5_object_key(key: &[u8], obj_id: ObjectId, salt: bool) -> ObjectKey {
    let mut extended = [0u8; 48];
    let mut len = key.len();
    extended[..len].copy_from_slice(key);
    extended[len..len + 3].copy_from_slice(&obj_id.0.to_le_bytes()[..3]);
    extended[len + 3..len + 5].copy_from_slice(&obj_id.1.to_le_bytes()[..2]);
    len += 5;
    if salt {
        extended[len..len + 4].copy_from_slice(b"sAlT");
        len += 4;
    }

    let key_len = (key.len() + 5).min(16);
    let mut bytes = [0u8; 32];
    bytes[..key_len].copy_from_slice(&Md5::digest(&extended[..len])[..key_len]);
    ObjectKey {
        bytes,
        len: key_len as u8,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Rc4CryptFilter;

impl CryptFilter for Rc4CryptFilter {
    fn method(&self) -> &[u8] {
        b"V2"
    }

    fn compute_key(&self, key: &[u8], obj_id: ObjectId) -> Result<ObjectKey, DecryptionError> {
        Ok(md5_object_key(key, obj_id, false))
    }

    fn encrypt(&self, key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, DecryptionError> {
        Ok(with_cached_rc4(key, |state| Rc4::encrypt_state(state, plaintext)))
    }

    fn decrypt(&self, key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, DecryptionError> {
        Ok(with_cached_rc4(key, |state| Rc4::decrypt_state(state, ciphertext)))
    }
}

/// AES-CBC with PKCS#5 padding (RFC 2898) under a key of the width the named
/// cipher takes. `Aes128CryptFilter` and `Aes256CryptFilter` agree on the
/// whole CBC envelope and differ only in key size and cipher, so it is
/// generated; `compute_key` stays per-filter because the two derive the key
/// differently (Algorithms 1 and 2.B respectively).
macro_rules! aes_cbc_crypt_filter {
    ($name:ident, $method:literal, $key_len:literal, $enc:ty, $dec:ty, $compute_key:expr) => {
        #[derive(Clone, Copy, Debug)]
        pub struct $name;

        impl CryptFilter for $name {
            fn method(&self) -> &[u8] {
                $method
            }

            fn compute_key(
                &self,
                key: &[u8],
                obj_id: ObjectId,
            ) -> Result<ObjectKey, DecryptionError> {
                $compute_key(key, obj_id)
            }

            fn encrypt(&self, key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, DecryptionError> {
                let key: &[u8; $key_len] = key
                    .try_into()
                    .map_err(|_| DecryptionError::InvalidKeyLength)?;

                // The ciphertext needs to be a multiple of 16 bytes to include the padding.
                let ciphertext_len = (plaintext.len() + 16) / 16 * 16;

                // Allocate sufficient bytes for the initialization vector, the ciphertext and the padding
                // combined.
                let mut ciphertext = Vec::with_capacity(16 + ciphertext_len);

                // Generate random numbers to populate the initialization vector.
                let mut rng = rand::rng();
                let mut iv = [0u8; 16];
                rng.fill(&mut iv);

                // Combine the IV and the plaintext.
                ciphertext.extend_from_slice(&iv);
                ciphertext.extend_from_slice(plaintext);
                ciphertext.resize(16 + ciphertext_len, 0);

                <$enc>::new(key.into(), &iv.into())
                    .encrypt_padded::<Pkcs5>(&mut ciphertext[16..], plaintext.len())
                    // Padding errors should not occur when encrypting, but avoid causing a panic.
                    .map_err(|_| DecryptionError::Padding)?;

                Ok(ciphertext)
            }

            fn decrypt(&self, key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, DecryptionError> {
                let key: &[u8; $key_len] = key
                    .try_into()
                    .map_err(|_| DecryptionError::InvalidKeyLength)?;

                // pdfium fails soft: a payload shorter than the IV decrypts
                // to empty, a partial trailing block is ignored, and invalid
                // PKCS#5 padding leaves the block unpadded rather than
                // dropping the object (cpdf_crypto_handler.cpp:211-233,
                // 301-304).
                if ciphertext.len() < 16 {
                    return Ok(vec![]);
                }
                let blocks = (ciphertext.len() - 16) / 16;
                if blocks == 0 {
                    return Ok(vec![]);
                }

                let mut iv = [0x00u8; 16];
                iv.copy_from_slice(&ciphertext[..16]);
                let aligned = &ciphertext[16..16 + blocks * 16];

                if let Ok(plain) = <$dec>::new(key.into(), &iv.into())
                    .decrypt_padded::<Pkcs5>(&mut aligned.to_vec())
                {
                    return Ok(plain.to_vec());
                }

                // Invalid padding: decrypt without removing any and drop the
                // count only when it is a plausible 1..16 tail.
                let mut plain = aligned.to_vec();
                <$dec>::new(key.into(), &iv.into())
                    .decrypt_padded::<NoPadding>(&mut plain)
                    .map_err(|_| DecryptionError::Padding)?;
                if let Some(&padding) = plain.last() {
                    if padding > 0 && padding < 16 {
                        let len = plain.len().saturating_sub(usize::from(padding));
                        plain.truncate(len);
                    }
                }
                Ok(plain)
            }
        }
    };
}

/// Algorithm 1: extend the file key with the object number and generation,
/// then, for AES, with the "sAlT" marker, and take the first min(n + 5, 16)
/// bytes of the MD5 as the key.
fn aes128_compute_key(key: &[u8], obj_id: ObjectId) -> Result<ObjectKey, DecryptionError> {
    // Algorithm 1 extends the file key with "sAlT" before the hash.
    Ok(md5_object_key(key, obj_id, true))
}

/// Algorithm 2.B: the 32-byte file encryption key is already the AES-256 key.
fn aes256_compute_key(key: &[u8], _obj_id: ObjectId) -> Result<ObjectKey, DecryptionError> {
    Ok(ObjectKey::new(key))
}

aes_cbc_crypt_filter!(
    Aes128CryptFilter,
    b"AESV2",
    16,
    Aes128CbcEnc,
    Aes128CbcDec,
    aes128_compute_key
);
aes_cbc_crypt_filter!(
    Aes256CryptFilter,
    b"AESV3",
    32,
    Aes256CbcEnc,
    Aes256CbcDec,
    aes256_compute_key
);

thread_local! {
    static LAST_RC4: std::cell::RefCell<Option<(ObjectKey, [u8; 256])>> =
        const { std::cell::RefCell::new(None) };
}

/// Many objects contain several strings that share one per-object key; reuse
/// the KSA state while the recursion visits them.  The cached key lives inline,
/// and the scratch state is copied only once per payload.
fn with_cached_rc4<T>(key: &[u8], transform: impl FnOnce(&mut [u8; 256]) -> T) -> T {
    LAST_RC4.with(|cache| {
        let mut cache = cache.borrow_mut();
        let mut state = match cache.as_ref() {
            Some((cached_key, state)) if cached_key.as_slice() == key => *state,
            _ => {
                let rc4 = Rc4::new(key);
                let state = rc4.initial_state();
                *cache = Some((ObjectKey::new(key), state));
                state
            }
        };
        transform(&mut state)
    })
}
