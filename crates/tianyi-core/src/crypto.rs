//! 天翼云盘加解密实现，移植自 OpenList `189pc/help.go`

use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;
use hmac::{Hmac, Mac};
use md5::Md5;
use regex::Regex;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use sha1::Sha1;
use std::sync::OnceLock;

/// HMAC-SHA1 类型别名
type HmacSha1 = Hmac<Sha1>;

/// 获取 HTTP 规范日期（RFC 1123，GMT，如 "Sun, 29 Aug 2026 12:34:56 GMT"）
pub fn http_date() -> String {
    use chrono::Utc;
    Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// 时间戳（毫秒）
pub fn timestamp_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 生成 clientSuffix 查询参数（与 Go 端 `clientSuffix()` 一致）
pub fn client_suffix() -> Vec<(String, String)> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let rand1 = rng.gen_range(0..100_000_i64);
    let rand2 = rng.gen_range(0..10_000_000_000_i64);
    vec![
        ("clientType".into(), "TELEPC".into()),
        ("version".into(), "6.2".into()),
        ("channelId".into(), "web_cloud.189.cn".into()),
        ("rand".into(), format!("{rand1}_{rand2}")),
    ]
}

/// 计算签名（与 Go 端 `signatureOfHmac` 一致）
pub fn signature(
    session_secret: &str,
    session_key: &str,
    operate: &str,
    full_url: &str,
    date_of_gmt: &str,
    params: &str,
) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"://[^/]+((/[^/\s?#]+)*)").expect("invalid urlpath regex")
    });

    let urlpath = re
        .captures(full_url)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let mut data = format!(
        "SessionKey={}&Operate={}&RequestURI={}&Date={}",
        session_key, operate, urlpath, date_of_gmt
    );
    if !params.is_empty() {
        data.push_str(&format!("&params={}", params));
    }

    let mut mac = <HmacSha1 as Mac>::new_from_slice(session_secret.as_bytes()).expect("hmac key");
    mac.update(data.as_bytes());
    hex::encode_upper(mac.finalize().into_bytes())
}

/// RSA 加密（与 Go 端 `RsaEncrypt` 一致，返回大写 hex）
///
/// `public_key` 为 `encryptConf.do` 返回的裸 PKIX/SPKI 公钥（base64），
/// OpenList 用 PEM 包裹后经 `ParsePKIXPublicKey` 解析。
pub fn rsa_encrypt(public_key: &str, orig_data: &str) -> Result<String, crate::error::Error> {
    let der = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        public_key.trim(),
    )
    .map_err(|e| crate::error::Error::Crypto(format!("base64 pkix: {e}")))?;
    let pub_key = RsaPublicKey::from_public_key_der(&der)
        .map_err(|e| crate::error::Error::Crypto(format!("pkix parse: {e}")))?;
    let mut rng = rand::thread_rng();
    let data = pub_key
        .encrypt(&mut rng, Pkcs1v15Encrypt, orig_data.as_bytes())
        .map_err(|e| crate::error::Error::Crypto(e.to_string()))?;
    Ok(hex::encode_upper(data))
}

/// PKCS7 填充
fn pkcs7_padding(data: &[u8], block_size: usize) -> Vec<u8> {
    let padding = block_size - data.len() % block_size;
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(padding as u8).take(padding));
    out
}

/// AES-ECB 加密（与 Go 端 `AesECBEncrypt` 一致，返回大写 hex）
pub fn aes_ecb_encrypt(data: &str, key: &str) -> Result<String, crate::error::Error> {
    let cipher = Aes128::new_from_slice(key.as_bytes())
        .map_err(|e| crate::error::Error::Crypto(e.to_string()))?;
    let padded = pkcs7_padding(data.as_bytes(), 16);
    let mut out = vec![0u8; padded.len()];
    for (src, dst) in padded.chunks(16).zip(out.chunks_mut(16)) {
        let mut block = *<&[u8; 16]>::try_from(src).unwrap();
        cipher.encrypt_block((&mut block).into());
        dst.copy_from_slice(&block);
    }
    Ok(hex::encode_upper(out))
}

/// MD5 计算（大写 hex）
pub fn md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    let mut h = Md5::new();
    Digest::update(&mut h, data);
    hex::encode_upper(h.finalize())
}

/// 对字符串拼接的 MD5（用于 sliceMd5 计算）
pub fn md5_of_joined(parts: &[&str]) -> String {
    md5_hex(parts.join("\n").as_bytes())
}

/// 计算分片大小（与 Go 端 `partSize` 一致）
pub fn part_size(size: i64) -> i64 {
    const DEFAULT: i64 = 1024 * 1024 * 10; // 10 MiB
    if size > DEFAULT * 2 * 999 {
        let multiplier = ((size as f64 / 1999.0 / DEFAULT as f64).ceil().max(5.0)) as i64;
        multiplier * DEFAULT
    } else if size > DEFAULT * 999 {
        DEFAULT * 2 // 20 MiB
    } else {
        DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_part_size() {
        assert_eq!(part_size(1024 * 1024), 10 * 1024 * 1024);
        assert_eq!(part_size(11 * 1024 * 1024 * 999), 20 * 1024 * 1024);
        // 超过 20MiB*999 后按 1999 分片向上取 10MiB 倍数
        let big = 11 * 1024 * 1024 * 2000; // 22000 MiB
        let multiplier = ((big as f64 / 1999.0 / (10 * 1024 * 1024) as f64).ceil().max(5.0)) as i64;
        assert_eq!(part_size(big), multiplier * 10 * 1024 * 1024);
    }

    #[test]
    fn test_md5() {
        assert_eq!(md5_hex(b"hello"), "5D41402ABC4B2A76B9719D911017C592");
    }

    #[test]
    fn test_aes_ecb_roundtrip_key() {
        // 验证 AES-ECB 可用（key 必须为 16 字节）
        let key = "0123456789abcdef"; // 16 bytes
        let data = "hello";
        let enc = aes_ecb_encrypt(data, key).unwrap();
        assert!(!enc.is_empty());
        // 结果应为大写 hex
        assert_eq!(enc.to_uppercase(), enc);
    }
}
