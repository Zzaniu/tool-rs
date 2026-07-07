use crate::unix_timestamp_secs;
use crate::AnyResult;
use anyhow::anyhow;
use http::HeaderMap;
use url::Url;
use uuid::Uuid;

pub enum SignatureAlgorithm {
    Blake3,
    HmacSha256,
}

impl SignatureAlgorithm {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "blake3" => Some(Self::Blake3),
            "sha256" => Some(Self::HmacSha256),
            _ => None,
        }
    }

    pub fn to_str(&self) -> &'static str {
        match self {
            Self::Blake3 => "blake3",
            Self::HmacSha256 => "sha256",
        }
    }
}

pub fn build_sorted_query(query_str: &str) -> String {
    if query_str.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, &str)> = query_str
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            Some((parts.next()?, parts.next().unwrap_or("")))
        })
        .collect();
    pairs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let mut result = String::new();
    for (k, v) in pairs {
        result.push_str(k);
        result.push_str(v);
    }
    result
}

pub fn build_signature_payload(
    algo: &str,
    method: &str,
    path: &str,
    sorted_query: &str,
    body: &[u8],
    nonce: &str,
    timestamp: &str,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(algo.as_bytes());
    payload.extend_from_slice(method.as_bytes());
    payload.extend_from_slice(path.as_bytes());
    payload.extend_from_slice(sorted_query.as_bytes());
    payload.extend_from_slice(body);
    payload.extend_from_slice(nonce.as_bytes());
    payload.extend_from_slice(timestamp.as_bytes());
    payload
}

pub fn compute_signature(
    algo: &SignatureAlgorithm,
    secret: &str,
    payload: &[u8],
) -> AnyResult<String> {
    match algo {
        SignatureAlgorithm::Blake3 => {
            if secret.len() < 32 {
                return Err(anyhow!("app_secret长度不足32字节"));
            }
            let key: &[u8; 32] = secret.as_bytes()[..32]
                .try_into()
                .map_err(|_| anyhow!("app_secret转换失败"))?;
            Ok(crate::crypto::hash_blake3::blake3_with_key(payload, key))
        }
        SignatureAlgorithm::HmacSha256 => {
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
                .map_err(|_| anyhow!("app_secret HMAC 初始化失败"))?;
            mac.update(payload);
            Ok(hex::encode(mac.finalize().into_bytes()))
        }
    }
}

pub struct SystemSigner<'a> {
    app_key: &'a str,
    app_secret: &'a str,
}

impl<'a> SystemSigner<'a> {
    pub fn new(app_key: &'a str, app_secret: &'a str) -> Self {
        Self {
            app_key,
            app_secret,
        }
    }

    pub fn build_signature_headers(
        &self,
        algo: &SignatureAlgorithm,
        method: &str,
        full_url: &str,
        body: &[u8],
    ) -> AnyResult<HeaderMap> {
        let parsed = Url::parse(full_url).map_err(|e| anyhow!("URL解析失败: {e}"))?;
        let path = parsed.path();
        let query = parsed.query().unwrap_or("");

        let sorted_query = build_sorted_query(query);
        let nonce = Uuid::new_v4().to_string();
        let timestamp = unix_timestamp_secs().to_string();

        let payload = build_signature_payload(
            algo.to_str(),
            method,
            path,
            &sorted_query,
            body,
            &nonce,
            &timestamp,
        );

        let hash = compute_signature(algo, self.app_secret, &payload)?;

        let mut headers = HeaderMap::with_capacity(5);
        headers.insert(
            "X-App-Key",
            self.app_key
                .try_into()
                .map_err(|e| anyhow!("X-App-Key header转换失败: {e}"))?,
        );
        headers.insert(
            "X-Signature",
            hash.try_into()
                .map_err(|e| anyhow!("X-Signature header转换失败: {e}"))?,
        );
        headers.insert(
            "X-Timestamp",
            timestamp
                .as_str()
                .try_into()
                .map_err(|e| anyhow!("X-Timestamp header转换失败: {e}"))?,
        );
        headers.insert(
            "X-Signature-Algorithm",
            algo.to_str()
                .try_into()
                .map_err(|e| anyhow!("X-Signature-Algorithm header转换失败: {e}"))?,
        );
        headers.insert(
            "X-Nonce",
            nonce
                .as_str()
                .try_into()
                .map_err(|e| anyhow!("X-Nonce header转换失败: {e}"))?,
        );

        Ok(headers)
    }

    pub fn build_signature_headers_with_blake3(
        &self,
        method: &str,
        full_url: &str,
        body: &[u8],
    ) -> AnyResult<HeaderMap> {
        self.build_signature_headers(&SignatureAlgorithm::Blake3, method, full_url, body)
    }

    pub fn build_signature_headers_with_sha256(
        &self,
        method: &str,
        full_url: &str,
        body: &[u8],
    ) -> AnyResult<HeaderMap> {
        self.build_signature_headers(&SignatureAlgorithm::HmacSha256, method, full_url, body)
    }
}

pub struct MultipartBuilder {
    boundary: String,
    body: Vec<u8>,
}

impl MultipartBuilder {
    pub fn new() -> Self {
        Self {
            boundary: Uuid::new_v4().as_simple().to_string(),
            body: Vec::new(),
        }
    }

    pub fn add_part(&mut self, name: &str, filename: Option<&str>, content: &[u8]) {
        self.body.extend_from_slice(b"--");
        self.body.extend_from_slice(self.boundary.as_bytes());
        self.body
            .extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
        self.body.extend_from_slice(name.as_bytes());
        if let Some(filename) = filename {
            self.body.extend_from_slice(b"\"; filename=\"");
            self.body.extend_from_slice(filename.as_bytes());
            self.body.push(b'"');
        }
        self.body.extend_from_slice(b"\"\r\n\r\n");
        self.body.extend_from_slice(content);
        self.body.extend_from_slice(b"\r\n");
    }

    pub fn finish(mut self) -> (String, Vec<u8>) {
        self.body.extend_from_slice(b"--");
        self.body.extend_from_slice(self.boundary.as_bytes());
        self.body.extend_from_slice(b"--\r\n");
        (self.boundary, self.body)
    }
}
