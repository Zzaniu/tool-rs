use crate::{unix_timestamp_millis, AnyResult};
use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit}, Aes128Gcm, Aes256Gcm, Key,
    Nonce,
};
use anyhow::anyhow;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use std::sync::{atomic, LazyLock};

pub use aes_gcm::{Aes128Gcm as Aes128GcmType, Aes256Gcm as Aes256GcmType};

#[derive(Default)]
struct NonceGenerator {
    no: atomic::AtomicU32,
}

impl NonceGenerator {
    /// 8 字节毫秒时间戳 + 4 字节流水号, 每毫秒最大 42 亿
    fn gen_unique_no(&self) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        let now_timestamp = unix_timestamp_millis() as u64;
        nonce[0..8].copy_from_slice(&now_timestamp.to_be_bytes());
        let no = self.no.fetch_add(1, atomic::Ordering::Relaxed);
        nonce[8..12].copy_from_slice(&no.to_be_bytes());
        nonce
    }
}

/// 全局唯一的序列号
fn gen_unique_no() -> [u8; 12] {
    static UNIQUE_NO_INFO: LazyLock<NonceGenerator> = LazyLock::new(|| NonceGenerator::default());
    UNIQUE_NO_INFO.gen_unique_no()
}

pub trait UniqueNoGenerator {
    fn gen_unique_no(&self) -> [u8; 12] {
        gen_unique_no()
    }
}

pub trait SecretGetter {
    /// Aes128Gcm 需要 16 字节, Aes256Gcm 需要 32 字节
    fn get_secret_key(&self) -> &[u8];
}

/// 核心泛型 AES-GCM trait，通过 `C` 指定 `Aes128Gcm` 或 `Aes256Gcm`。
pub trait AesGcm<C: Aead + KeyInit>: UniqueNoGenerator + SecretGetter {
    /// gcm 加密, 12 字节 nonce + 密文
    fn encrypt(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        let key = Key::<C>::try_from(self.get_secret_key())?;
        let cipher = C::new(&key);
        let unique_no = self.gen_unique_no();
        // 必须全局唯一
        let nonce = Nonce::<<C as AeadCore>::NonceSize>::try_from(unique_no.as_slice())?;

        let ciphertext = cipher
            .encrypt(&nonce, data)
            .map_err(|e| anyhow!("encrypt_gcm failure! err: {e}"))?;
        let mut payload = nonce.to_vec();
        payload.extend_from_slice(&ciphertext);
        Ok(payload)
    }

    fn encrypt_base64(&self, data: &[u8]) -> AnyResult<String> {
        Ok(BASE64_STANDARD.encode(self.encrypt(data)?))
    }

    fn encrypt_hex(&self, data: &[u8]) -> AnyResult<String> {
        Ok(hex::encode(self.encrypt(data)?))
    }

    /// gcm 解密
    fn decrypt(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        if data.len() < 12 {
            anyhow::bail!("invalid length");
        }
        let (nonce_part, encrypted_part) = data.split_at(12);
        let key = Key::<C>::try_from(self.get_secret_key())?;
        let cipher = C::new(&key);
        let nonce = Nonce::<<C as AeadCore>::NonceSize>::try_from(nonce_part)?;

        cipher
            .decrypt(&nonce, encrypted_part)
            .map_err(|e| anyhow!("decrypt failure! err: {e}").into())
    }

    fn decrypt_base64(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        let data = BASE64_STANDARD
            .decode(data)
            .map_err(|e| anyhow!("decrypt_gcm failure! err: {e}"))?;
        self.decrypt(&data)
    }

    fn decrypt_hex(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        let data = hex::decode(data).map_err(|e| anyhow!("decrypt_gcm failure! err: {e}"))?;
        self.decrypt(&data)
    }
}

impl<C: Aead + KeyInit, T: UniqueNoGenerator + SecretGetter> AesGcm<C> for T {}

// ============ 语法糖：Aes128Gcm ============

pub trait TAes128Gcm: UniqueNoGenerator + SecretGetter + Sized {
    fn encrypt_aes128(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        <Self as AesGcm<Aes128Gcm>>::encrypt(self, data)
    }

    fn encrypt_aes128_base64(&self, data: &[u8]) -> AnyResult<String> {
        <Self as AesGcm<Aes128Gcm>>::encrypt_base64(self, data)
    }

    fn encrypt_aes128_hex(&self, data: &[u8]) -> AnyResult<String> {
        <Self as AesGcm<Aes128Gcm>>::encrypt_hex(self, data)
    }

    fn decrypt_aes128(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        <Self as AesGcm<Aes128Gcm>>::decrypt(self, data)
    }

    fn decrypt_aes128_base64(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        <Self as AesGcm<Aes128Gcm>>::decrypt_base64(self, data)
    }

    fn decrypt_aes128_hex(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        <Self as AesGcm<Aes128Gcm>>::decrypt_hex(self, data)
    }
}

impl<T: UniqueNoGenerator + SecretGetter> TAes128Gcm for T {}

// ============ 语法糖：Aes256Gcm ============

pub trait TAes256Gcm: UniqueNoGenerator + SecretGetter + Sized {
    fn encrypt_aes256(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        <Self as AesGcm<Aes256Gcm>>::encrypt(self, data)
    }

    fn encrypt_aes256_base64(&self, data: &[u8]) -> AnyResult<String> {
        <Self as AesGcm<Aes256Gcm>>::encrypt_base64(self, data)
    }

    fn encrypt_aes256_hex(&self, data: &[u8]) -> AnyResult<String> {
        <Self as AesGcm<Aes256Gcm>>::encrypt_hex(self, data)
    }

    fn decrypt_aes256(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        <Self as AesGcm<Aes256Gcm>>::decrypt(self, data)
    }

    fn decrypt_aes256_base64(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        <Self as AesGcm<Aes256Gcm>>::decrypt_base64(self, data)
    }

    fn decrypt_aes256_hex(&self, data: &[u8]) -> AnyResult<Vec<u8>> {
        <Self as AesGcm<Aes256Gcm>>::decrypt_hex(self, data)
    }
}

impl<T: UniqueNoGenerator + SecretGetter> TAes256Gcm for T {}

#[cfg(test)]
mod tests {
    use super::*;
    struct AAA {
        key: &'static str,
    }

    impl AAA {
        pub fn new(key: &'static str) -> Self {
            Self { key }
        }
    }

    impl UniqueNoGenerator for AAA {}
    impl SecretGetter for AAA {
        fn get_secret_key(&self) -> &[u8] {
            self.key.as_bytes()
        }
    }

    #[test]
    fn test_aes128_gcm() {
        let aaa = AAA::new("1234567812345678");
        let s = aaa.encrypt_aes128_base64(b"test").unwrap();
        let vec = aaa.decrypt_aes128_base64(s.as_bytes()).unwrap();
        assert_eq!(&vec, b"test");
    }

    #[test]
    fn test_aes256_gcm() {
        let aaa = AAA::new("12345678123456781234567812345678");
        let s = aaa.encrypt_aes256_base64(b"test").unwrap();
        let vec = aaa.decrypt_aes256_base64(s.as_bytes()).unwrap();
        assert_eq!(&vec, b"test");
    }
}
