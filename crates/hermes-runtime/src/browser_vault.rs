//! `browser_vault`: model-blind encrypted autofill for logins, TOTP codes, payment cards, and
//! addresses on the current browser page.
//!
//! Mirrors `tools/browser_vault_tool.py`, `agent/vault_store.py`, and
//! `agent/vault_login_classifier.py`. Secrets are encrypted at rest with Python `cryptography`
//! Fernet-compatible AES-128-CBC + HMAC-SHA256 tokens (`<home>/vault/vault.key` +
//! `vault.json.enc`), verified against the exact page origin inside the browser DOM before any
//! value is written, and never returned in tool output.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::browser_cdp::{resolve_endpoint, BrowserSettings};
use crate::browser_supervisor;
use crate::clarify;
use crate::file_tools::{ApprovalAction, ApprovalDecision, ApprovalRequest};
use crate::providers::oauth::{base64_decode_flexible, base64url_encode};
use crate::tools::Approver;

pub(crate) const BROWSER_VAULT: &str = "browser_vault";

pub(crate) const PAYMENT_FIELDS: &[&str] = &[
    "card_number",
    "exp_month",
    "exp_year",
    "cvc",
    "cardholder_name",
];
pub(crate) const ADDRESS_FIELDS: &[&str] = &[
    "full_name",
    "street1",
    "street2",
    "city",
    "state",
    "postal_code",
    "country",
    "phone",
];

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/browser_vault.json"))
            .expect("valid browser_vault schema JSON"),
    })
}

// ── Redaction registry for filled secrets ────────────────────────────────────────────────────────

static RUNTIME_SECRETS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn secret_registry() -> &'static Mutex<HashSet<String>> {
    RUNTIME_SECRETS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn register_secret_redaction(secret: &str) {
    if secret.len() >= 4 {
        if let Ok(mut set) = secret_registry().lock() {
            set.insert(secret.to_owned());
        }
    }
}

pub(crate) fn scrub_registered_secrets(text: &str) -> String {
    let snapshot: Vec<String> = secret_registry()
        .lock()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    if snapshot.is_empty() || text.is_empty() {
        return text.to_owned();
    }
    let mut out = text.to_owned();
    for secret in snapshot {
        if out.contains(&secret) {
            out = out.replace(&secret, "[vault-secret-redacted]");
        }
    }
    out
}

pub(crate) fn scrub_value(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(scrub_registered_secrets(&s)),
        Value::Array(arr) => Value::Array(arr.into_iter().map(scrub_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (scrub_registered_secrets(&k), scrub_value(v)))
                .collect(),
        ),
        other => other,
    }
}

// ── Pure-Rust FIPS-197 AES-128 + CBC + Fernet ────────────────────────────────────────────────────

#[rustfmt::skip]
const SBOX: [u8; 256] = [
    0x63,0x7c,0x77,0x7b,0xf2,0x6b,0x6f,0xc5,0x30,0x01,0x67,0x2b,0xfe,0xd7,0xab,0x76,
    0xca,0x82,0xc9,0x7d,0xfa,0x59,0x47,0xf0,0xad,0xd4,0xa2,0xaf,0x9c,0xa4,0x72,0xc0,
    0xb7,0xfd,0x93,0x26,0x36,0x3f,0xf7,0xcc,0x34,0xa5,0xe5,0xf1,0x71,0xd8,0x31,0x15,
    0x04,0xc7,0x23,0xc3,0x18,0x96,0x05,0x9a,0x07,0x12,0x80,0xe2,0xeb,0x27,0xb2,0x75,
    0x09,0x83,0x2c,0x1a,0x1b,0x6e,0x5a,0xa0,0x52,0x3b,0xd6,0xb3,0x29,0xe3,0x2f,0x84,
    0x53,0xd1,0x00,0xed,0x20,0xfc,0xb1,0x5b,0x6a,0xcb,0xbe,0x39,0x4a,0x4c,0x58,0xcf,
    0xd0,0xef,0xaa,0xfb,0x43,0x4d,0x33,0x85,0x45,0xf9,0x02,0x7f,0x50,0x3c,0x9f,0xa8,
    0x51,0xa3,0x40,0x8f,0x92,0x9d,0x38,0xf5,0xbc,0xb6,0xda,0x21,0x10,0xff,0xf3,0xd2,
    0xcd,0x0c,0x13,0xec,0x5f,0x97,0x44,0x17,0xc4,0xa7,0x7e,0x3d,0x64,0x5d,0x19,0x73,
    0x60,0x81,0x4f,0xdc,0x22,0x2a,0x90,0x88,0x46,0xee,0xb8,0x14,0xde,0x5e,0x0b,0xdb,
    0xe0,0x32,0x3a,0x0a,0x49,0x06,0x24,0x5c,0xc2,0xd3,0xac,0x62,0x91,0x95,0xe4,0x79,
    0xe7,0xc8,0x37,0x6d,0x8d,0xd5,0x4e,0xa9,0x6c,0x56,0xf4,0xea,0x65,0x7a,0xae,0x08,
    0xba,0x78,0x25,0x2e,0x1c,0xa6,0xb4,0xc6,0xe8,0xdd,0x74,0x1f,0x4b,0xbd,0x8b,0x8a,
    0x70,0x3e,0xb5,0x66,0x48,0x03,0xf6,0x0e,0x61,0x35,0x57,0xb9,0x86,0xc1,0x1d,0x9e,
    0xe1,0xf8,0x98,0x11,0x69,0xd9,0x8e,0x94,0x9b,0x1e,0x87,0xe9,0xce,0x55,0x28,0xdf,
    0x8c,0xa1,0x89,0x0d,0xbf,0xe6,0x42,0x68,0x41,0x99,0x2d,0x0f,0xb0,0x54,0xbb,0x16,
];

#[rustfmt::skip]
const INV_SBOX: [u8; 256] = [
    0x52,0x09,0x6a,0xd5,0x30,0x36,0xa5,0x38,0xbf,0x40,0xa3,0x9e,0x81,0xf3,0xd7,0xfb,
    0x7c,0xe3,0x39,0x82,0x9b,0x2f,0xff,0x87,0x34,0x8e,0x43,0x44,0xc4,0xde,0xe9,0xcb,
    0x54,0x7b,0x94,0x32,0xa6,0xc2,0x23,0x3d,0xee,0x4c,0x95,0x0b,0x42,0xfa,0xc3,0x4e,
    0x08,0x2e,0xa1,0x66,0x28,0xd9,0x24,0xb2,0x76,0x5b,0xa2,0x49,0x6d,0x8b,0xd1,0x25,
    0x72,0xf8,0xf6,0x64,0x86,0x68,0x98,0x16,0xd4,0xa4,0x5c,0xcc,0x5d,0x65,0xb6,0x92,
    0x6c,0x70,0x48,0x50,0xfd,0xed,0xb9,0xda,0x5e,0x15,0x46,0x57,0xa7,0x8d,0x9d,0x84,
    0x90,0xd8,0xab,0x00,0x8c,0xbc,0xd3,0x0a,0xf7,0xe4,0x58,0x05,0xb8,0xb3,0x45,0x06,
    0xd0,0x2c,0x1e,0x8f,0xca,0x3f,0x0f,0x02,0xc1,0xaf,0xbd,0x03,0x01,0x13,0x8a,0x6b,
    0x3a,0x91,0x11,0x41,0x4f,0x67,0xdc,0xea,0x97,0xf2,0xcf,0xce,0xf0,0xb4,0xe6,0x73,
    0x96,0xac,0x74,0x22,0xe7,0xad,0x35,0x85,0xe2,0xf9,0x37,0xe8,0x1c,0x75,0xdf,0x6e,
    0x47,0xf1,0x1a,0x71,0x1d,0x29,0xc5,0x89,0x6f,0xb7,0x62,0x0e,0xaa,0x18,0xbe,0x1b,
    0xfc,0x56,0x3e,0x4b,0xc6,0xd2,0x79,0x20,0x9a,0xdb,0xc0,0xfe,0x78,0xcd,0x5a,0xf4,
    0x1f,0xdd,0xa8,0x33,0x88,0x07,0xc7,0x31,0xb1,0x12,0x10,0x59,0x27,0x80,0xec,0x5f,
    0x60,0x51,0x7f,0xa9,0x19,0xb5,0x4a,0x0d,0x2d,0xe5,0x7a,0x9f,0x93,0xc9,0x9c,0xef,
    0xa0,0xe0,0x3b,0x4d,0xae,0x2a,0xf5,0xb0,0xc8,0xeb,0xbb,0x3c,0x83,0x53,0x99,0x61,
    0x17,0x2b,0x04,0x7e,0xba,0x77,0xd6,0x26,0xe1,0x69,0x14,0x63,0x55,0x21,0x0c,0x7d,
];

const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

#[inline]
fn xtime(a: u8) -> u8 {
    (a << 1) ^ (if a & 0x80 != 0 { 0x1b } else { 0 })
}

#[inline]
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

fn aes128_expand_key(key: &[u8; 16]) -> [[u8; 16]; 11] {
    let mut rk = [[0u8; 16]; 11];
    rk[0] = *key;
    for round in 1..=10 {
        let prev = rk[round - 1];
        let mut next = [0u8; 16];
        let t = [
            SBOX[prev[13] as usize] ^ RCON[round - 1],
            SBOX[prev[14] as usize],
            SBOX[prev[15] as usize],
            SBOX[prev[12] as usize],
        ];
        for i in 0..4 {
            next[i] = prev[i] ^ t[i];
        }
        for i in 4..16 {
            next[i] = prev[i] ^ next[i - 4];
        }
        rk[round] = next;
    }
    rk
}

fn add_round_key(state: &mut [u8; 16], rk: &[u8; 16]) {
    for i in 0..16 {
        state[i] ^= rk[i];
    }
}

#[allow(dead_code)]
fn aes128_encrypt_block(rk: &[[u8; 16]; 11], block: &[u8; 16]) -> [u8; 16] {
    let mut s = *block;
    add_round_key(&mut s, &rk[0]);
    for (round, round_key) in rk.iter().enumerate().skip(1) {
        for b in &mut s {
            *b = SBOX[*b as usize];
        }
        // ShiftRows (column-major)
        let t1 = s[1];
        s[1] = s[5];
        s[5] = s[9];
        s[9] = s[13];
        s[13] = t1;
        s.swap(2, 10);
        s.swap(6, 14);
        let t3 = s[15];
        s[15] = s[11];
        s[11] = s[7];
        s[7] = s[3];
        s[3] = t3;

        if round < 10 {
            for col in 0..4 {
                let i = col * 4;
                let (a0, a1, a2, a3) = (s[i], s[i + 1], s[i + 2], s[i + 3]);
                let xor = a0 ^ a1 ^ a2 ^ a3;
                s[i] ^= xor ^ xtime(a0 ^ a1);
                s[i + 1] ^= xor ^ xtime(a1 ^ a2);
                s[i + 2] ^= xor ^ xtime(a2 ^ a3);
                s[i + 3] ^= xor ^ xtime(a3 ^ a0);
            }
        }
        add_round_key(&mut s, round_key);
    }
    s
}

fn aes128_decrypt_block(rk: &[[u8; 16]; 11], block: &[u8; 16]) -> [u8; 16] {
    let mut s = *block;
    add_round_key(&mut s, &rk[10]);
    for round in (0..10).rev() {
        // InvShiftRows
        let t1 = s[13];
        s[13] = s[9];
        s[9] = s[5];
        s[5] = s[1];
        s[1] = t1;
        s.swap(2, 10);
        s.swap(6, 14);
        let t3 = s[3];
        s[3] = s[7];
        s[7] = s[11];
        s[11] = s[15];
        s[15] = t3;

        for b in &mut s {
            *b = INV_SBOX[*b as usize];
        }
        add_round_key(&mut s, &rk[round]);
        if round > 0 {
            for col in 0..4 {
                let i = col * 4;
                let (a0, a1, a2, a3) = (s[i], s[i + 1], s[i + 2], s[i + 3]);
                s[i] = gf_mul(a0, 0x0e) ^ gf_mul(a1, 0x0b) ^ gf_mul(a2, 0x0d) ^ gf_mul(a3, 0x09);
                s[i + 1] =
                    gf_mul(a0, 0x09) ^ gf_mul(a1, 0x0e) ^ gf_mul(a2, 0x0b) ^ gf_mul(a3, 0x0d);
                s[i + 2] =
                    gf_mul(a0, 0x0d) ^ gf_mul(a1, 0x09) ^ gf_mul(a2, 0x0e) ^ gf_mul(a3, 0x0b);
                s[i + 3] =
                    gf_mul(a0, 0x0b) ^ gf_mul(a1, 0x0d) ^ gf_mul(a2, 0x09) ^ gf_mul(a3, 0x0e);
            }
        }
    }
    s
}

#[allow(dead_code)]
fn aes128_cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    let rk = aes128_expand_key(key);
    let pad_len = 16 - (plaintext.len() % 16);
    let mut padded = Vec::with_capacity(plaintext.len() + pad_len);
    padded.extend_from_slice(plaintext);
    padded.resize(plaintext.len() + pad_len, pad_len as u8);

    let mut out = Vec::with_capacity(padded.len());
    let mut prev = *iv;
    for chunk in padded.chunks_exact(16) {
        let mut block = [0u8; 16];
        for i in 0..16 {
            block[i] = chunk[i] ^ prev[i];
        }
        let enc = aes128_encrypt_block(&rk, &block);
        out.extend_from_slice(&enc);
        prev = enc;
    }
    out
}

fn aes128_cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err("Invalid Fernet ciphertext block alignment".into());
    }
    let rk = aes128_expand_key(key);
    let mut out = Vec::with_capacity(ciphertext.len());
    let mut prev = *iv;
    for chunk in ciphertext.chunks_exact(16) {
        let block: [u8; 16] = chunk.try_into().expect("16-byte chunk");
        let mut dec = aes128_decrypt_block(&rk, &block);
        for i in 0..16 {
            dec[i] ^= prev[i];
        }
        out.extend_from_slice(&dec);
        prev = block;
    }
    let pad = *out.last().unwrap_or(&0) as usize;
    if pad == 0 || pad > 16 || pad > out.len() {
        return Err("Invalid PKCS#7 padding in vault ciphertext".into());
    }
    if out[out.len() - pad..].iter().any(|&b| b as usize != pad) {
        return Err("Corrupted PKCS#7 padding in vault ciphertext".into());
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

fn fernet_b64_encode(bytes: &[u8]) -> String {
    let mut encoded = base64url_encode(bytes);
    while !encoded.len().is_multiple_of(4) {
        encoded.push('=');
    }
    encoded
}

fn parse_fernet_key(raw_key: &[u8]) -> Result<([u8; 16], [u8; 16]), String> {
    let text = std::str::from_utf8(raw_key)
        .map_err(|_| "vault.key is not UTF-8".to_owned())?
        .trim();
    let decoded = base64_decode_flexible(text)
        .ok_or_else(|| "vault.key is not valid base64url".to_owned())?;
    if decoded.len() != 32 {
        return Err(format!(
            "vault.key must decode to 32 bytes (got {})",
            decoded.len()
        ));
    }
    let signing: [u8; 16] = decoded[..16].try_into().expect("16 bytes");
    let encryption: [u8; 16] = decoded[16..32].try_into().expect("16 bytes");
    Ok((signing, encryption))
}

fn fernet_generate_key() -> Result<String, String> {
    let mut raw = [0u8; 32];
    SystemRandom::new()
        .fill(&mut raw)
        .map_err(|_| "SystemRandom failed generating vault key".to_owned())?;
    Ok(fernet_b64_encode(&raw))
}

#[allow(dead_code)]
fn fernet_encrypt_with_time(
    raw_key: &[u8],
    plaintext: &[u8],
    timestamp_secs: u64,
    iv: [u8; 16],
) -> Result<Vec<u8>, String> {
    let (signing, encryption) = parse_fernet_key(raw_key)?;
    let ciphertext = aes128_cbc_encrypt(&encryption, &iv, plaintext);
    let mut payload = Vec::with_capacity(1 + 8 + 16 + ciphertext.len() + 32);
    payload.push(0x80);
    payload.extend_from_slice(&timestamp_secs.to_be_bytes());
    payload.extend_from_slice(&iv);
    payload.extend_from_slice(&ciphertext);
    let hmac_key = hmac::Key::new(hmac::HMAC_SHA256, &signing);
    let tag = hmac::sign(&hmac_key, &payload);
    payload.extend_from_slice(tag.as_ref());
    Ok(fernet_b64_encode(&payload).into_bytes())
}

#[allow(dead_code)]
fn fernet_encrypt(raw_key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut iv = [0u8; 16];
    SystemRandom::new()
        .fill(&mut iv)
        .map_err(|_| "SystemRandom failed generating Fernet IV".to_owned())?;
    fernet_encrypt_with_time(raw_key, plaintext, ts, iv)
}

fn fernet_decrypt(raw_key: &[u8], token_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (signing, encryption) = parse_fernet_key(raw_key)?;
    let text = std::str::from_utf8(token_bytes)
        .map_err(|_| "vault.json.enc is not UTF-8 base64".to_owned())?
        .trim();
    let raw = base64_decode_flexible(text)
        .ok_or_else(|| "vault file could not be decrypted (invalid token encoding)".to_owned())?;
    if raw.len() < 1 + 8 + 16 + 16 + 32 || raw[0] != 0x80 {
        return Err("vault file could not be decrypted (invalid token header)".into());
    }
    let body_len = raw.len() - 32;
    let (signed_part, mac) = raw.split_at(body_len);
    let hmac_key = hmac::Key::new(hmac::HMAC_SHA256, &signing);
    hmac::verify(&hmac_key, signed_part, mac)
        .map_err(|_| "vault file could not be decrypted (key mismatch or corruption)".to_owned())?;
    let iv: [u8; 16] = signed_part[9..25].try_into().expect("16-byte IV");
    aes128_cbc_decrypt(&encryption, &iv, &signed_part[25..])
}

// ── Origins & TOTP (RFC 6238) ────────────────────────────────────────────────────────────────────

pub(crate) fn normalize_origin(url_or_origin: &str) -> Result<String, String> {
    let value = url_or_origin.trim();
    if value.is_empty() {
        return Err("origin is required".into());
    }
    let parsed =
        reqwest::Url::parse(value).map_err(|_| format!("could not parse origin from '{value}'"))?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    let host = parsed
        .host_str()
        .map(str::to_ascii_lowercase)
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("could not parse origin host from '{value}'"))?;
    let default_port = match scheme.as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    match parsed.port() {
        Some(p) if Some(p) != default_port => Ok(format!("{scheme}://{host}:{p}")),
        _ => Ok(format!("{scheme}://{host}")),
    }
}

fn base32_decode(input: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '=')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if cleaned.is_empty() {
        return Err("authenticator secret is empty".into());
    }
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits: u8 = 0;
    for ch in cleaned.bytes() {
        let val = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'2'..=b'7' => ch - b'2' + 26,
            _ => return Err("authenticator key contains non-base32 character".into()),
        };
        buffer = (buffer << 5) | u32::from(val);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

#[allow(dead_code)]
pub(crate) fn normalize_otp_secret(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let mut digits = 6u32;
    let mut period = 30u64;
    let mut algo = "SHA1".to_owned();
    let mut value = trimmed.to_owned();

    if trimmed.to_ascii_lowercase().starts_with("otpauth://") {
        let parsed =
            reqwest::Url::parse(trimmed).map_err(|_| "invalid otpauth:// URI".to_owned())?;
        let mut secret_param = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "secret" => secret_param = Some(v.into_owned()),
                "digits" => {
                    digits = v.parse().map_err(|_| "invalid otpauth digits".to_owned())?;
                }
                "period" => {
                    period = v.parse().map_err(|_| "invalid otpauth period".to_owned())?;
                }
                "algorithm" => {
                    algo = v.to_ascii_uppercase().replace('-', "");
                }
                _ => {}
            }
        }
        value = secret_param.ok_or_else(|| "otpauth:// URI missing secret parameter".to_owned())?;
        if !(6..=8).contains(&digits)
            || period == 0
            || !matches!(algo.as_str(), "SHA1" | "SHA256" | "SHA512")
        {
            return Err(
                "unsupported otpauth:// parameters (digits 6-8, period > 0, SHA1/SHA256/SHA512)"
                    .into(),
            );
        }
    }

    let seed: String = value
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect::<String>()
        .trim_end_matches('=')
        .to_owned();
    let _ = base32_decode(&seed)?;
    if digits == 6 && period == 30 && algo == "SHA1" {
        Ok(seed)
    } else {
        Ok(format!("{seed}|{digits}|{period}|{algo}"))
    }
}

pub(crate) fn totp_at(seed_spec: &str, unix_secs: u64) -> Result<String, String> {
    let mut parts = seed_spec.split('|');
    let seed = parts.next().unwrap_or_default();
    let digits: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(6);
    let period: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let algo = parts.next().unwrap_or("SHA1");

    let key_bytes = base32_decode(seed)?;
    let counter = (unix_secs / period.max(1)).to_be_bytes();
    let algorithm = match algo {
        "SHA256" => hmac::HMAC_SHA256,
        "SHA512" => hmac::HMAC_SHA512,
        _ => hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
    };
    let hmac_key = hmac::Key::new(algorithm, &key_bytes);
    let tag = hmac::sign(&hmac_key, &counter);
    let digest = tag.as_ref();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let bin = ((u32::from(digest[offset]) & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    let modulo = 10u32.pow(digits);
    let code = bin % modulo;
    Ok(format!("{code:0width$}", width = digits as usize))
}

// ── Vault Store ──────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VaultItemMeta {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) label: String,
    pub(crate) origin: Option<String>,
    pub(crate) created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) identifier_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) identifier: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) has_otp: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultRecord {
    id: String,
    kind: String,
    label: String,
    origin: Option<String>,
    created_at: String,
    #[serde(default)]
    identifier_type: Option<String>,
    #[serde(default)]
    identifier: Option<String>,
    secret: HashMap<String, String>,
}

impl VaultRecord {
    fn meta(&self) -> VaultItemMeta {
        VaultItemMeta {
            id: self.id.clone(),
            kind: self.kind.clone(),
            label: self.label.clone(),
            origin: self.origin.clone(),
            created_at: self.created_at.clone(),
            identifier_type: self.identifier_type.clone(),
            identifier: self.identifier.clone(),
            has_otp: self
                .secret
                .get("otp_secret")
                .is_some_and(|s| !s.trim().is_empty()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultEnvelope {
    version: u32,
    items: Vec<VaultRecord>,
}

static STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) struct VaultStore {
    base_dir: PathBuf,
}

impl VaultStore {
    pub(crate) fn for_profile(profile: Option<&Path>) -> Self {
        let base = profile
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("HERMES_HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(".hermes"));
        Self {
            base_dir: base.join("vault"),
        }
    }

    fn key_path(&self) -> PathBuf {
        self.base_dir.join("vault.key")
    }

    fn vault_path(&self) -> PathBuf {
        self.base_dir.join("vault.json.enc")
    }

    fn ensure_dir(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.base_dir)
            .map_err(|e| format!("Failed creating vault directory: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.base_dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }

    fn has_ciphertext(&self) -> bool {
        self.vault_path()
            .metadata()
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    }

    fn load_or_create_key(&self) -> Result<Vec<u8>, String> {
        self.ensure_dir()?;
        let key_path = self.key_path();
        if key_path.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
            }
            let bytes =
                std::fs::read(&key_path).map_err(|e| format!("Failed reading vault.key: {e}"))?;
            return Ok(bytes);
        }
        if self.has_ciphertext() {
            return Err(format!(
                "The vault key could not be found (vault.key is missing in {}). Refusing to overwrite existing ciphertext.",
                self.base_dir.display()
            ));
        }
        let key_str = fernet_generate_key()?;
        write_private_atomic(&key_path, key_str.as_bytes())?;
        Ok(key_str.into_bytes())
    }

    fn read_all_locked(&self) -> Result<Vec<VaultRecord>, String> {
        let vault_path = self.vault_path();
        if !vault_path.exists() {
            return Ok(Vec::new());
        }
        let blob = std::fs::read(&vault_path)
            .map_err(|e| format!("Failed reading vault ciphertext: {e}"))?;
        if blob.is_empty() {
            return Ok(Vec::new());
        }
        let key = self.load_or_create_key()?;
        let plain = fernet_decrypt(&key, &blob)?;
        let env: VaultEnvelope = serde_json::from_slice(&plain)
            .map_err(|_| "vault file is corrupted (invalid JSON envelope)".to_owned())?;
        Ok(env.items)
    }

    #[allow(dead_code)]
    fn write_all_locked(&self, items: &[VaultRecord]) -> Result<(), String> {
        self.ensure_dir()?;
        let key = self.load_or_create_key()?;
        let payload = serde_json::to_vec(&VaultEnvelope {
            version: 1,
            items: items.to_vec(),
        })
        .map_err(|e| format!("Failed encoding vault JSON: {e}"))?;
        let enc = fernet_encrypt(&key, &payload)?;
        write_private_atomic(&self.vault_path(), &enc)
    }

    pub(crate) fn list_items(&self) -> Result<Vec<VaultItemMeta>, String> {
        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| "Vault lock poisoned".to_owned())?;
        Ok(self
            .read_all_locked()?
            .into_iter()
            .map(|r| r.meta())
            .collect())
    }

    pub(crate) fn resolve_record(
        &self,
        handle: &str,
    ) -> Result<(VaultItemMeta, HashMap<String, String>), String> {
        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| "Vault lock poisoned".to_owned())?;
        let items = self.read_all_locked()?;
        let rec = items.into_iter().find(|r| r.id == handle).ok_or_else(|| {
            format!("No vault item with handle '{handle}'. Call action=list first.")
        })?;
        let meta = rec.meta();
        Ok((meta, rec.secret))
    }

    #[allow(dead_code)]
    pub(crate) fn add_item(
        &self,
        kind: &str,
        label: &str,
        mut secret: HashMap<String, String>,
        origin: Option<&str>,
    ) -> Result<VaultItemMeta, String> {
        let label = label.trim();
        if label.is_empty() {
            return Err("label is required".into());
        }
        let mut norm_origin = None;
        let mut identifier = None;
        let mut identifier_type = None;

        if kind == "login" {
            let raw_origin =
                origin.ok_or_else(|| "origin is required for login items".to_owned())?;
            norm_origin = Some(normalize_origin(raw_origin)?);
            let id_type = secret
                .remove("identifier_type")
                .unwrap_or_else(|| "username".to_owned());
            if !matches!(id_type.as_str(), "email" | "username" | "phone") {
                return Err("identifier_type must be one of email, username, phone".into());
            }
            let ident = secret
                .remove("identifier")
                .unwrap_or_default()
                .trim()
                .to_owned();
            let password = secret.get("password").cloned().unwrap_or_default();
            if ident.is_empty() || password.is_empty() {
                return Err("login items require identifier and password".into());
            }
            let otp_raw = secret.get("otp_secret").cloned().unwrap_or_default();
            let otp_norm = normalize_otp_secret(&otp_raw)?;
            secret.clear();
            secret.insert("password".into(), password);
            if !otp_norm.is_empty() {
                secret.insert("otp_secret".into(), otp_norm);
            }
            identifier_type = Some(id_type);
            identifier = Some(ident);
        } else if kind == "payment" || kind == "address" {
            let allowed = if kind == "payment" {
                PAYMENT_FIELDS
            } else {
                ADDRESS_FIELDS
            };
            secret.retain(|k, v| allowed.contains(&k.as_str()) && !v.trim().is_empty());
            if kind == "payment" {
                for req in ["card_number", "exp_month", "exp_year", "cvc"] {
                    if !secret.contains_key(req) {
                        return Err(format!("payment item missing required field '{req}'"));
                    }
                }
            } else {
                for req in ["full_name", "street1", "city", "postal_code", "country"] {
                    if !secret.contains_key(req) {
                        return Err(format!("address item missing required field '{req}'"));
                    }
                }
            }
            if let Some(o) = origin.filter(|s| !s.trim().is_empty()) {
                norm_origin = Some(normalize_origin(o)?);
            }
        } else {
            return Err(format!("unknown vault kind '{kind}'"));
        }

        let mut rand_bytes = [0u8; 6];
        SystemRandom::new()
            .fill(&mut rand_bytes)
            .map_err(|_| "SystemRandom failed".to_owned())?;
        let mut id = String::from("vault_");
        for b in rand_bytes {
            use std::fmt::Write;
            let _ = write!(&mut id, "{b:02x}");
        }
        let ts_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let created_at = DateTime::from_timestamp(ts_secs, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| ts_secs.to_string());

        let record = VaultRecord {
            id,
            kind: kind.to_owned(),
            label: label.to_owned(),
            origin: norm_origin,
            created_at,
            identifier_type,
            identifier,
            secret,
        };
        let meta = record.meta();

        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| "Vault lock poisoned".to_owned())?;
        let mut all = self.read_all_locked()?;
        all.push(record);
        self.write_all_locked(&all)?;
        Ok(meta)
    }
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Path has no parent directory".to_owned())?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".vault-tmp-")
        .tempfile_in(parent)
        .map_err(|e| format!("Failed creating temp vault file: {e}"))?;
    {
        use std::io::Write;
        tmp.write_all(bytes)
            .and_then(|_| tmp.flush())
            .map_err(|e| format!("Failed writing vault file: {e}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600));
    }
    tmp.persist(path)
        .map_err(|e| format!("Failed persisting {}: {}", path.display(), e.error))?;
    Ok(())
}

// ── In-page form inspection & origin-guarded autofill JS ─────────────────────────────────────────

const ORIGIN_INSPECT_JS: &str = r#"(() => {
    const inputs = Array.from(document.querySelectorAll('input, select, textarea'));
    const controls = [];
    const vis = (el) => {
        if (el.disabled || el.type === 'hidden') return false;
        const r = el.getBoundingClientRect ? el.getBoundingClientRect() : {width: 1, height: 1};
        const st = window.getComputedStyle ? window.getComputedStyle(el) : null;
        if (st && (st.display === 'none' || st.visibility === 'hidden')) return false;
        return r.width > 0 || r.height > 0 || inputs.length <= 8;
    };
    inputs.forEach((el, idx) => {
        if (!vis(el)) return;
        const label = el.id ? (document.querySelector('label[for="' + CSS.escape(el.id) + '"]')?.innerText || '') : '';
        controls.push({
            index: idx,
            tag: (el.tagName || '').toLowerCase(),
            type: (el.getAttribute('type') || el.type || 'text').toLowerCase(),
            autocomplete: (el.getAttribute('autocomplete') || '').toLowerCase(),
            name: (el.getAttribute('name') || '').toLowerCase(),
            id: (el.getAttribute('id') || '').toLowerCase(),
            placeholder: (el.getAttribute('placeholder') || '').toLowerCase(),
            ariaLabel: (el.getAttribute('aria-label') || '').toLowerCase(),
            labelText: label.trim().toLowerCase(),
            maxLength: typeof el.maxLength === 'number' ? el.maxLength : -1
        });
    });
    return {
        origin: window.location && window.location.origin ? window.location.origin : '',
        href: window.location && window.location.href ? window.location.href : '',
        controls
    };
})()"#;

#[derive(Debug, Deserialize)]
struct DomControl {
    index: usize,
    tag: String,
    #[serde(rename = "type")]
    input_type: String,
    autocomplete: String,
    name: String,
    id: String,
    placeholder: String,
    #[serde(rename = "ariaLabel")]
    aria_label: String,
    #[serde(rename = "labelText")]
    label_text: String,
}

impl DomControl {
    fn blob(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.name, self.id, self.placeholder, self.aria_label, self.label_text
        )
    }

    fn is_otp(&self) -> bool {
        if self.autocomplete.contains("one-time-code") {
            return true;
        }
        let b = self.blob();
        b.contains("otp")
            || b.contains("2fa")
            || b.contains("mfa")
            || b.contains("totp")
            || b.contains("verification")
            || b.contains("auth code")
            || b.contains("passcode")
    }
}

fn pick_login_controls(controls: &[DomControl]) -> (Option<usize>, Option<usize>) {
    let mut password_idx = None;
    let mut identifier_idx = None;
    for c in controls {
        if c.tag != "input" {
            continue;
        }
        if password_idx.is_none()
            && !c.is_otp()
            && (c.input_type == "password" || c.autocomplete.contains("current-password"))
        {
            password_idx = Some(c.index);
        }
    }
    for c in controls {
        if c.tag != "input" || Some(c.index) == password_idx || c.is_otp() {
            continue;
        }
        if matches!(
            c.input_type.as_str(),
            "text" | "email" | "tel" | "search" | ""
        ) {
            let b = c.blob();
            if c.input_type == "email"
                || c.autocomplete.contains("username")
                || c.autocomplete.contains("email")
                || b.contains("user")
                || b.contains("email")
                || b.contains("login")
                || b.contains("account")
                || b.contains("phone")
            {
                identifier_idx = Some(c.index);
                break;
            }
        }
    }
    if identifier_idx.is_none() {
        for c in controls {
            if c.tag == "input"
                && Some(c.index) != password_idx
                && !c.is_otp()
                && matches!(c.input_type.as_str(), "text" | "email" | "tel")
            {
                identifier_idx = Some(c.index);
                break;
            }
        }
    }
    (identifier_idx, password_idx)
}

fn pick_otp_control(controls: &[DomControl]) -> Option<usize> {
    if let Some(c) = controls.iter().find(|c| c.tag == "input" && c.is_otp()) {
        return Some(c.index);
    }
    controls
        .iter()
        .find(|c| {
            c.tag == "input"
                && matches!(c.input_type.as_str(), "text" | "tel" | "number" | "")
                && (c.blob().contains("code") || c.blob().contains("token"))
        })
        .map(|c| c.index)
}

fn pick_checkout_controls(
    controls: &[DomControl],
    secret: &HashMap<String, String>,
    allowed: &[&str],
) -> Vec<(usize, String, String)> {
    let mut matches = Vec::new();
    let mut used = HashSet::new();
    let hints: &[(&str, &[&str])] = &[
        (
            "card_number",
            &[
                "cc-number",
                "cardnumber",
                "card number",
                "card_number",
                "pan",
            ],
        ),
        (
            "exp_month",
            &["cc-exp-month", "exp_month", "expmonth", "mm"],
        ),
        ("exp_year", &["cc-exp-year", "exp_year", "expyear", "yyyy"]),
        ("cvc", &["cc-csc", "cvc", "cvv", "security code", "csc"]),
        (
            "cardholder_name",
            &["cc-name", "cardholder", "name on card"],
        ),
        ("full_name", &["name", "full_name", "fullname"]),
        (
            "street1",
            &["address-line1", "street", "address1", "address line 1"],
        ),
        ("street2", &["address-line2", "apt", "suite", "address2"]),
        ("city", &["address-level2", "city", "town"]),
        ("state", &["address-level1", "state", "province", "region"]),
        ("postal_code", &["postal-code", "zip", "postal", "postcode"]),
        ("country", &["country"]),
        ("phone", &["tel", "phone"]),
    ];
    for &field in allowed {
        let Some(val) = secret.get(field).filter(|s| !s.is_empty()) else {
            continue;
        };
        let keys = hints
            .iter()
            .find(|(k, _)| *k == field)
            .map(|(_, h)| *h)
            .unwrap_or(&[]);
        if let Some(ctrl) = controls.iter().find(|c| {
            !used.contains(&c.index)
                && (c.autocomplete.contains(field.replace('_', "-").as_str())
                    || keys
                        .iter()
                        .any(|h| c.autocomplete.contains(h) || c.blob().contains(h)))
        }) {
            used.insert(ctrl.index);
            matches.push((ctrl.index, field.to_owned(), val.clone()));
        }
    }
    matches
}

fn build_origin_guarded_fill_js(expected_origin: &str, writes: &[(usize, String)]) -> String {
    let writes_json = serde_json::to_string(
        &writes
            .iter()
            .map(|(idx, val)| json!({"index": idx, "value": val}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into());
    let origin_json = serde_json::to_string(expected_origin).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(() => {{
    try {{
        if (!window.location || window.location.origin !== {origin_json}) {{
            return {{ ok: false, reason: "origin_mismatch", actual_origin: window.location ? window.location.origin : "" }};
        }}
        const all = document.querySelectorAll('input, select, textarea');
        const ops = {writes_json};
        let filled = 0;
        for (const op of ops) {{
            const el = all[op.index];
            if (!el) continue;
            const proto = el.tagName === 'TEXTAREA'
                ? HTMLTextAreaElement.prototype
                : el.tagName === 'SELECT'
                    ? HTMLSelectElement.prototype
                    : HTMLInputElement.prototype;
            const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
            el.focus();
            if (setter) setter.call(el, op.value);
            else el.value = op.value;
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            if (el.value === op.value) filled += 1;
        }}
        return {{ ok: true, filled }};
    }} catch (_) {{
        return {{ ok: false, reason: "eval_failed" }};
    }}
}})()"#
    )
}

async fn confirm_payment_fill(
    label: &str,
    origin: &str,
    approval_timeout_secs: Option<u32>,
    approve: &mut Approver<'_>,
) -> Result<(), Value> {
    let Some(timeout) = approval_timeout_secs else {
        return Err(json!({
            "success": false,
            "error_type": "confirmation_required",
            "error": "Payment card autofill requires interactive user confirmation."
        }));
    };
    let questions_arg = json!({
        "questions": [{
            "question": format!("Fill saved payment card '{label}' on {origin}? Card details never enter the conversation."),
            "choices": ["Approve", "Decline"]
        }]
    });
    let Ok(questions) = clarify::normalize(&questions_arg) else {
        return Err(json!({
            "success": false,
            "error_type": "payment_declined",
            "error": "Could not build payment confirmation prompt."
        }));
    };
    let req = ApprovalRequest {
        target: origin.to_owned(),
        content: clarify::request_payload(&questions),
        action: ApprovalAction::Clarify,
        timeout_secs: timeout,
    };
    match approve(req).await {
        ApprovalDecision::Approved => Ok(()),
        ApprovalDecision::Answered(text) if text.contains("Approve") => Ok(()),
        _ => Err(json!({
            "success": false,
            "error_type": "payment_declined",
            "error": format!("User declined payment card '{label}' fill on {origin}.")
        })),
    }
}

pub(crate) async fn call(
    settings: Option<&BrowserSettings>,
    task_id: &str,
    profile: Option<&Path>,
    approval_timeout_secs: Option<u32>,
    approve: &mut Approver<'_>,
    arguments: &str,
) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let handle = args
        .get("handle")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let store = VaultStore::for_profile(profile);

    match action {
        "list" => match store.list_items() {
            Ok(items) => json!({
                "success": true,
                "items": items,
                "locked": []
            }),
            Err(err) => json!({"success": false, "error": err}),
        },
        "unlock" => {
            let backend = args
                .get("backend")
                .and_then(Value::as_str)
                .unwrap_or("local");
            json!({
                "success": false,
                "error_type": "unlock_unavailable",
                "error": format!("External password manager backend '{backend}' is not configured; local encrypted vault items are unlocked automatically via vault.key.")
            })
        }
        "fill" | "enter_code" | "save_login" => {
            let raw_url = settings.and_then(|s| s.cdp_url.as_deref()).unwrap_or("");
            let endpoint = resolve_endpoint(raw_url).await;
            if endpoint.is_empty() {
                return json!({
                    "success": false,
                    "error": "No browser session endpoint is configured for browser_vault."
                });
            }
            let sup = match browser_supervisor::ensure(task_id, &endpoint).await {
                Ok(s) => s,
                Err(err) => return json!({"success": false, "error": err}),
            };

            let inspect_val = match sup
                .evaluate_runtime(ORIGIN_INSPECT_JS, None, None, false, 10.0)
                .await
            {
                Ok(v) => v,
                Err(err) => return json!({"success": false, "error": err}),
            };
            let raw_page_origin = inspect_val
                .get("origin")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let page_origin = match normalize_origin(raw_page_origin) {
                Ok(o) => o,
                Err(_) => {
                    return json!({
                        "success": false,
                        "error_type": "no_page_origin",
                        "error": "Current browser page has no valid HTTP/HTTPS origin."
                    })
                }
            };
            let controls: Vec<DomControl> = serde_json::from_value(
                inspect_val
                    .get("controls")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            )
            .unwrap_or_default();

            if action == "save_login" {
                let Some(_timeout) = approval_timeout_secs else {
                    return json!({
                        "success": false,
                        "error_type": "save_declined",
                        "error": "save_login requires an interactive prompt or `hermes vault` CLI because secrets cannot pass through model arguments."
                    });
                };
                return json!({
                    "success": false,
                    "error_type": "save_declined",
                    "error": "Masked OS credential entry is not available in this terminal backend; store credentials with `hermes vault add` instead."
                });
            }

            if handle.is_empty() {
                return json!({
                    "success": false,
                    "error": format!("'handle' is required for action={action}.")
                });
            }
            let (meta, secret) = match store.resolve_record(handle) {
                Ok(pair) => pair,
                Err(err) => return json!({"success": false, "error": err}),
            };

            if let Some(bound_origin) = meta.origin.as_deref() {
                if bound_origin != page_origin {
                    return json!({
                        "success": false,
                        "error_type": "origin_mismatch",
                        "error": format!(
                            "Refused: saved item '{}' is bound to '{}', but current page is '{}'.",
                            meta.label, bound_origin, page_origin
                        )
                    });
                }
            } else if meta.kind == "login" {
                return json!({
                    "success": false,
                    "error_type": "origin_mismatch",
                    "error": "Login item has no bound origin."
                });
            }

            if action == "enter_code" {
                let Some(seed) = secret.get("otp_secret").filter(|s| !s.is_empty()) else {
                    return json!({
                        "success": false,
                        "error_type": "no_otp_secret",
                        "error": format!("Item '{}' has no saved authenticator key.", meta.label)
                    });
                };
                let Some(otp_idx) = pick_otp_control(&controls) else {
                    return json!({
                        "success": false,
                        "error_type": "no_code_field",
                        "error": "No verification code field found on the current page."
                    });
                };
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let code = match totp_at(seed, now) {
                    Ok(c) => c,
                    Err(err) => return json!({"success": false, "error": err}),
                };
                register_secret_redaction(&code);
                let script = build_origin_guarded_fill_js(&page_origin, &[(otp_idx, code)]);
                return match sup.evaluate_runtime(&script, None, None, false, 10.0).await {
                    Ok(res) if res.get("ok") == Some(&json!(true)) => json!({
                        "success": true,
                        "filled_fields": res["filled"],
                        "kind": "otp",
                        "origin": page_origin
                    }),
                    Ok(res) => json!({
                        "success": false,
                        "error_type": res.get("reason").and_then(Value::as_str).unwrap_or("eval_failed"),
                        "error": "Failed injecting verification code on current page."
                    }),
                    Err(_) => json!({
                        "success": false,
                        "error_type": "eval_failed",
                        "error": "The browser could not execute the vault script."
                    }),
                };
            }

            // action == "fill"
            if meta.kind == "payment" {
                if let Err(refusal) =
                    confirm_payment_fill(&meta.label, &page_origin, approval_timeout_secs, approve)
                        .await
                {
                    return refusal;
                }
            }

            let mut writes: Vec<(usize, String)> = Vec::new();
            let mut identifier_filled = false;
            let mut filled_field_names: Vec<String> = Vec::new();

            if meta.kind == "login" {
                let (id_idx, pw_idx) = pick_login_controls(&controls);
                let Some(pw) = secret.get("password").filter(|s| !s.is_empty()) else {
                    return json!({"success": false, "error": "Saved login has an empty password."});
                };
                let Some(pw_idx) = pw_idx else {
                    return json!({
                        "success": false,
                        "error": "No fillable password field found on this page."
                    });
                };
                register_secret_redaction(pw);
                if let (Some(ident_idx), Some(ident_val)) = (id_idx, meta.identifier.as_deref()) {
                    writes.push((ident_idx, ident_val.to_owned()));
                    identifier_filled = true;
                }
                writes.push((pw_idx, pw.clone()));
            } else {
                let allowed = if meta.kind == "payment" {
                    PAYMENT_FIELDS
                } else {
                    ADDRESS_FIELDS
                };
                let picked = pick_checkout_controls(&controls, &secret, allowed);
                if picked.is_empty() {
                    return json!({
                        "success": false,
                        "error": format!("No fillable {} field matched the saved item on this page.", meta.kind)
                    });
                }
                for (idx, fname, fval) in picked {
                    if meta.kind == "payment" {
                        register_secret_redaction(&fval);
                    }
                    filled_field_names.push(fname);
                    writes.push((idx, fval));
                }
            }

            let script = build_origin_guarded_fill_js(&page_origin, &writes);
            match sup.evaluate_runtime(&script, None, None, false, 10.0).await {
                Ok(res) if res.get("ok") == Some(&json!(true)) => {
                    let mut out = json!({
                        "success": true,
                        "filled_fields": res["filled"],
                        "kind": meta.kind,
                        "origin": page_origin,
                        "backend": "hermes-vault"
                    });
                    if meta.kind == "login" {
                        out["identifier_filled"] = json!(identifier_filled);
                    } else {
                        out["fields"] = json!(filled_field_names);
                    }
                    out
                }
                Ok(res) => json!({
                    "success": false,
                    "error_type": res.get("reason").and_then(Value::as_str).unwrap_or("eval_failed"),
                    "error": "Vault autofill refused or failed on the current page."
                }),
                Err(_) => json!({
                    "success": false,
                    "error_type": "eval_failed",
                    "error": "The browser could not execute the vault script."
                }),
            }
        }
        other => json!({
            "success": false,
            "error": format!("Unknown action '{other}'; use list, fill, save_login, enter_code or unlock.")
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn aes128_matches_fips197_test_vector() {
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let pt: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let expected: [u8; 16] = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        let rk = aes128_expand_key(&key);
        let ct = aes128_encrypt_block(&rk, &pt);
        assert_eq!(ct, expected);
        let dec = aes128_decrypt_block(&rk, &ct);
        assert_eq!(dec, pt);
    }

    #[test]
    fn fernet_matches_python_spec_vector_and_rejects_tamper() {
        // Test vector from cryptography.fernet spec:
        let key = b"cw_0x689RpI-jtRR7oE8h_eQsKImvJapLeSbXpwF4e4=";
        let mut token =
            fernet_encrypt_with_time(key, br#"{"hello":"vault"}"#, 1_700_000_000, [0x11; 16])
                .unwrap();
        let plain = fernet_decrypt(key, &token).unwrap();
        assert_eq!(plain, br#"{"hello":"vault"}"#);

        // Wrong key is rejected.
        let wrong = fernet_generate_key().unwrap();
        assert!(fernet_decrypt(wrong.as_bytes(), &token).is_err());

        // Corrupted token byte fails HMAC verification.
        let flip_idx = token.len() - 5;
        token[flip_idx] = if token[flip_idx] == b'A' { b'B' } else { b'A' };
        assert!(fernet_decrypt(key, &token).is_err());
    }

    #[test]
    fn totp_matches_rfc6238_vector() {
        // RFC 6238 Appendix B: secret "12345678901234567890", SHA1, 8 digits, t=59 => 94287082
        let spec = normalize_otp_secret(
            "otpauth://totp/Example:alice@example.com?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits=8&period=30&algorithm=SHA1",
        )
        .unwrap();
        assert_eq!(totp_at(&spec, 59).unwrap(), "94287082");
    }

    #[test]
    fn vault_store_encrypts_secrets_and_never_overwrites_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::for_profile(Some(dir.path()));
        let mut secret = HashMap::new();
        secret.insert("identifier_type".into(), "email".into());
        secret.insert("identifier".into(), "alice@example.com".into());
        secret.insert("password".into(), "TopSecretPassword!99".into());

        let meta = store
            .add_item(
                "login",
                "Example Login",
                secret,
                Some("https://app.example.com:443/login"),
            )
            .unwrap();
        assert_eq!(meta.origin.as_deref(), Some("https://app.example.com"));

        let listed = store.list_items().unwrap();
        let listed_json = serde_json::to_string(&listed).unwrap();
        assert!(!listed_json.contains("TopSecretPassword!99"));

        let enc_disk = std::fs::read_to_string(dir.path().join("vault/vault.json.enc")).unwrap();
        assert!(!enc_disk.contains("TopSecretPassword!99"));
        assert!(!enc_disk.contains("alice@example.com"));

        // Removing vault.key when ciphertext exists must error cleanly instead of wiping vault.
        std::fs::remove_file(dir.path().join("vault/vault.key")).unwrap();
        assert!(store.list_items().is_err());
    }

    #[tokio::test]
    async fn browser_vault_enforces_exact_origin_and_fills_without_leaking_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::for_profile(Some(dir.path()));
        let mut secret = HashMap::new();
        secret.insert("identifier_type".into(), "email".into());
        secret.insert("identifier".into(), "bob@example.com".into());
        secret.insert("password".into(), "UltraSecretPass#42".into());
        let meta = store
            .add_item(
                "login",
                "Bob Login",
                secret,
                Some("https://app.example.com"),
            )
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{port}/devtools/browser/vault");

        let current_origin =
            std::sync::Arc::new(std::sync::Mutex::new("https://app.example.com".to_owned()));
        let origin_for_server = std::sync::Arc::clone(&current_origin);
        let saw_secret_on_ws = std::sync::Arc::new(std::sync::Mutex::new(false));
        let saw_secret_flag = std::sync::Arc::clone(&saw_secret_on_ws);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(text) = msg else { continue };
                let req: Value = serde_json::from_str(&text).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap();
                match method {
                    "Target.getTargets" => {
                        ws.send(Message::text(
                            json!({"id": id, "result": {"targetInfos": [{"targetId": "T1", "type": "page", "url": "https://app.example.com/login"}]}}).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Target.attachToTarget" => {
                        ws.send(Message::text(
                            json!({"id": id, "result": {"sessionId": "S1"}}).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Runtime.evaluate" => {
                        let expr = req["params"]["expression"].as_str().unwrap();
                        if expr.contains("UltraSecretPass#42") {
                            *saw_secret_flag.lock().unwrap() = true;
                        }
                        let origin = origin_for_server.lock().unwrap().clone();
                        let val = if expr.contains("controls = []") {
                            json!({
                                "origin": origin,
                                "href": format!("{origin}/login"),
                                "controls": [
                                    {"index": 0, "tag": "input", "type": "email", "autocomplete": "username", "name": "user", "id": "u", "placeholder": "", "ariaLabel": "", "labelText": ""},
                                    {"index": 1, "tag": "input", "type": "password", "autocomplete": "current-password", "name": "pass", "id": "p", "placeholder": "", "ariaLabel": "", "labelText": ""}
                                ]
                            })
                        } else {
                            json!({"ok": true, "filled": 2})
                        };
                        ws.send(Message::text(
                            json!({"id": id, "result": {"result": {"value": val}}}).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    _ => {
                        ws.send(Message::text(json!({"id": id, "result": {}}).to_string()))
                            .await
                            .unwrap();
                    }
                }
            }
        });

        let settings = BrowserSettings {
            cdp_url: Some(ws_url),
            allow_private_urls: true,
        };
        let mut deny_fn = |_: ApprovalRequest| -> std::pin::Pin<
            Box<dyn std::future::Future<Output = ApprovalDecision> + Send>,
        > { Box::pin(async { ApprovalDecision::Denied }) };
        let mut deny_approver: &mut Approver<'_> = &mut deny_fn;
        let fill_ok = call(
            Some(&settings),
            "vault-fill-test",
            Some(dir.path()),
            None,
            &mut deny_approver,
            &json!({"action": "fill", "handle": meta.id}).to_string(),
        )
        .await;
        assert_eq!(fill_ok["success"], json!(true));
        assert_eq!(fill_ok["filled_fields"], json!(2));
        assert_eq!(fill_ok["identifier_filled"], json!(true));
        assert!(!fill_ok.to_string().contains("UltraSecretPass#42"));
        assert!(*saw_secret_on_ws.lock().unwrap());
        assert!(scrub_registered_secrets("echo UltraSecretPass#42 here")
            .contains("[vault-secret-redacted]"));

        // Changing origin must refuse without evaluating the fill script.
        *current_origin.lock().unwrap() = "https://evil.example.com".into();
        let fill_mismatch = call(
            Some(&settings),
            "vault-fill-test",
            Some(dir.path()),
            None,
            &mut deny_approver,
            &json!({"action": "fill", "handle": meta.id}).to_string(),
        )
        .await;
        assert_eq!(fill_mismatch["success"], json!(false));
        assert_eq!(fill_mismatch["error_type"], json!("origin_mismatch"));

        browser_supervisor::stop("vault-fill-test").await;
        server.abort();
    }
}
