use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyInit};
use anyhow::{anyhow, Result as AnyResult};
use base64::engine::general_purpose;
use base64::Engine;
pub use hex;

pub type Aes128EcbEnc = ecb::Encryptor<aes::Aes128>;
pub type Aes192EcbEnc = ecb::Encryptor<aes::Aes192>;
pub type Aes256EcbEnc = ecb::Encryptor<aes::Aes256>;
pub type Aes128EcbDec = ecb::Decryptor<aes::Aes128>;
pub type Aes192EcbDec = ecb::Decryptor<aes::Aes192>;
pub type Aes256EcbDec = ecb::Decryptor<aes::Aes256>;

pub fn ecb_encrypt<C, F>(key: impl AsRef<[u8]>, text: impl AsRef<[u8]>, f: F) -> AnyResult<String>
where
    C: KeyInit + BlockEncryptMut,
    F: Fn(&[u8]) -> String,
{
    let text = text.as_ref();
    let mut buf = vec![0u8; text.len() + 16];
    buf[..text.len()].copy_from_slice(text);
    let ct = C::new(key.as_ref().into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, text.len()) // pkcs7 补充明文
        .map_err(|err| anyhow!("Aes encrypt error, err = {:?}", err))?;
    Ok(f(ct))
}

pub fn ecb_encrypt_base64<C: KeyInit + BlockEncryptMut>(
    key: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
) -> AnyResult<String> {
    ecb_encrypt::<C, _>(key, text, |data| general_purpose::STANDARD.encode(data))
}

pub fn ecb_encrypt_hex<C: KeyInit + BlockEncryptMut>(
    key: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
) -> AnyResult<String> {
    ecb_encrypt::<C, _>(key, text, |data| hex::encode(data))
}

pub fn ecb_decrypt<C, F>(key: impl AsRef<[u8]>, text: impl AsRef<[u8]>, f: F) -> AnyResult<String>
where
    C: KeyInit + BlockDecryptMut,
    F: Fn(&[u8]) -> AnyResult<Vec<u8>>,
{
    let mut buf = f(text.as_ref())?;
    let ct = C::new(key.as_ref().into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|err| anyhow!("Aes encrypt error, err = {:?}", err))?;
    Ok(String::from_utf8_lossy(ct).into_owned())
}

pub fn ecb_decrypt_hex<C: KeyInit + BlockDecryptMut>(
    key: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
) -> AnyResult<String> {
    ecb_decrypt::<C, _>(key, text, |data| {
        hex::decode(data).map_err(|err| anyhow::anyhow!("hex encode error, err = {}", err))
    })
}

pub fn ecb_decrypt_base64<C: KeyInit + BlockDecryptMut>(
    key: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
) -> AnyResult<String> {
    ecb_decrypt::<C, _>(key, text, |data| {
        general_purpose::STANDARD
            .decode(data)
            .map_err(|err| anyhow!("base64 decode error, err = {}", err))
    })
}
