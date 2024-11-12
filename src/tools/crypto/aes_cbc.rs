use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use anyhow::{anyhow, Result as AnyResult};
use base64::engine::general_purpose;
use base64::Engine;
use hex;

pub type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
pub type Aes192CbcEnc = cbc::Encryptor<aes::Aes192>;
pub type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
pub type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
pub type Aes192CbcDec = cbc::Decryptor<aes::Aes192>;
pub type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

pub fn cbc_encrypt<C, F>(
    key: impl AsRef<[u8]>,
    iv: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
    f: F,
) -> AnyResult<String>
where
    C: KeyIvInit + BlockEncryptMut,
    F: Fn(&[u8]) -> String,
{
    let text = text.as_ref();
    let mut buf = vec![0u8; text.len() + 16];
    buf[..text.len()].copy_from_slice(text);
    let ct = C::new(key.as_ref().into(), iv.as_ref().into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, text.len()) // pkcs7 补充明文
        .map_err(|err| anyhow!("Aes encrypt error, err = {:?}", err))?;
    Ok(f(ct))
}

pub fn cbc_encrypt_base64<C: KeyIvInit + BlockEncryptMut>(
    key: impl AsRef<[u8]>,
    iv: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
) -> AnyResult<String> {
    cbc_encrypt::<C, _>(key, iv, text, |data| general_purpose::STANDARD.encode(data))
}

pub fn cbc_encrypt_hex<C: KeyIvInit + BlockEncryptMut>(
    key: impl AsRef<[u8]>,
    iv: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
) -> AnyResult<String> {
    cbc_encrypt::<C, _>(key, iv, text, |data| hex::encode(data))
}

pub fn cbc_decrypt<C, F>(
    key: impl AsRef<[u8]>,
    iv: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
    f: F,
) -> AnyResult<String>
where
    C: KeyIvInit + BlockDecryptMut,
    F: Fn(&[u8]) -> AnyResult<Vec<u8>>,
{
    let mut buf = f(text.as_ref())?;
    let ct = C::new(key.as_ref().into(), iv.as_ref().into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|err| anyhow!("Aes encrypt error, err = {:?}", err))?;
    Ok(String::from_utf8_lossy(ct).into_owned())
}

pub fn cbc_decrypt_hex<C: KeyIvInit + BlockDecryptMut>(
    key: impl AsRef<[u8]>,
    iv: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
) -> AnyResult<String> {
    cbc_decrypt::<C, _>(key, iv, text, |data| {
        hex::decode(data).map_err(|err| anyhow::anyhow!("hex encode error, err = {}", err))
    })
}

pub fn cbc_decrypt_base64<C: KeyIvInit + BlockDecryptMut>(
    key: impl AsRef<[u8]>,
    iv: impl AsRef<[u8]>,
    text: impl AsRef<[u8]>,
) -> AnyResult<String> {
    cbc_decrypt::<C, _>(key, iv, text, |data| {
        general_purpose::STANDARD
            .decode(data)
            .map_err(|err| anyhow!("base64 decode error, err = {}", err))
    })
}
