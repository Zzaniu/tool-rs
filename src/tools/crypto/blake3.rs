pub use blake3::Hash;

const SECRET: &str = "UUZqRqjmY@AylZ0$3h9GNeZc4z$*FC19(8JDCnpx";

/// blake3 哈希函数, 使用默认KEY
pub fn blake3(input: impl AsRef<[u8]>) -> String {
    blake3_to_hash(input).to_hex().to_string()
}

/// blake3 哈希函数, 使用自定义KEY
pub fn blake3_with_key(input: impl AsRef<[u8]>, key: &[u8; 32]) -> String {
    blake3::keyed_hash(key, input.as_ref()).to_hex().to_string()
}

pub fn blake3_to_hash(input: impl AsRef<[u8]>) -> Hash {
    blake3::keyed_hash(SECRET[..32].as_bytes().try_into().unwrap(), input.as_ref())
}
