//! WeChat Official Account (公众号) channel.
//!
//! Inbound: WeChat pushes encrypted XML to the gateway webhook
//! (`/webhooks/wechatmp`); the message is decrypted and routed through the
//! inbound pipeline by `gateway/webhooks.rs`. Outbound: replies are delivered
//! asynchronously via the customer-service message API (`/cgi-bin/message/
//! custom/send`), which is required because LLM responses routinely exceed
//! WeChat's 5-second passive-reply window.
//!
//! This module owns the channel adapter (outbound) plus the WeChat encrypted-
//! message protocol (signature verification, AES-256-CBC decrypt/encrypt, and
//! XML parsing) shared with the webhook handler.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use async_trait::async_trait;
use base64::Engine as _;
use rand::Rng as _;
use sha1::{Digest, Sha1};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::channels::{
    Channel, ChannelCapabilities, ChatType, ConversationId, Id, OutgoingMessage,
};
use crate::error::SyscityError;

const WECHAT_API_BASE: &str = "https://api.weixin.qq.com";
const TOKEN_EXPIRY_SECS: u64 = 7200;
const TOKEN_REFRESH_BUFFER_SECS: u64 = 300;

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// Credentials for a WeChat Official Account.
#[derive(Debug, Clone)]
pub struct WechatMpConfig {
    /// Public account AppID.
    pub app_id: String,
    /// AppSecret for the customer-service API.
    pub app_secret: String,
    /// Token configured in the official-account console (signature only).
    pub token: String,
    /// 43-char base64 EncodingAESKey (message encryption).
    pub encoding_aes_key: String,
}

impl WechatMpConfig {
    /// Derive the 32-byte AES key from the 43-char EncodingAESKey.
    pub fn aes_key(&self) -> crate::Result<[u8; 32]> {
        aes_key_from_encoding_key(&self.encoding_aes_key)
    }
}

// ─────────────────────────────────────────────
// Encrypted-message protocol (shared with webhook handler)
// ─────────────────────────────────────────────

/// Decode the 43-char base64 EncodingAESKey into the 32-byte AES key.
pub fn aes_key_from_encoding_key(encoding_aes_key: &str) -> crate::Result<[u8; 32]> {
    // 43 chars is not a multiple of 4; WeChat's key decodes with one `=`.
    // The final char's low 2 bits are padding and need not be zero, so the
    // strict STANDARD engine would reject otherwise-valid keys.
    let padded = format!("{encoding_aes_key}=");
    use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
    let engine = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
    );
    let decoded = engine
        .decode(&padded)
        .map_err(|e| SyscityError::Validation(format!("invalid EncodingAESKey: {e}")))?;
    decoded
        .try_into()
        .map_err(|_| SyscityError::Validation("EncodingAESKey must decode to 32 bytes".to_string()))
}

/// WeChat message signature: `sha1( sort([token, timestamp, nonce, encrypt]).join("") )`.
pub fn compute_signature(token: &str, timestamp: &str, nonce: &str, encrypt: &str) -> String {
    let mut parts = [token, timestamp, nonce, encrypt];
    parts.sort_unstable();
    let joined = parts.join("");
    let mut hasher = Sha1::new();
    hasher.update(joined.as_bytes());
    hex::encode(hasher.finalize())
}

/// Verify the webhook GET `echostr` signature (no `encrypt` component).
pub fn verify_echo_signature(token: &str, timestamp: &str, nonce: &str, provided: &str) -> bool {
    let mut parts = [token, timestamp, nonce];
    parts.sort_unstable();
    let joined = parts.join("");
    let mut hasher = Sha1::new();
    hasher.update(joined.as_bytes());
    let expected = hex::encode(hasher.finalize());
    expected == provided
}

/// AES-256-CBC decrypt a WeChat `Encrypt` payload (base64) to the plaintext
/// `random(16) + msg_len(4 BE) + msg_xml + appid`.
pub fn decrypt_message(key: &[u8; 32], ciphertext_b64: &str) -> crate::Result<Vec<u8>> {
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(ciphertext_b64)
        .map_err(|e| SyscityError::Validation(format!("invalid Encrypt base64: {e}")))?;
    let mut buf = ciphertext.clone();
    let decrypted = Aes256CbcDec::new(key.into(), key[..16].into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| SyscityError::Validation("AES decrypt failed (bad EncodingAESKey?)".into()))?
        .to_vec();
    Ok(decrypted)
}

/// Split the decrypted plaintext into `(msg_xml_bytes, appid)` per WeChat's
/// framing: `random(16) || len(4 BE) || msg || appid`.
pub fn parse_plaintext(plain: &[u8]) -> crate::Result<(Vec<u8>, String)> {
    if plain.len() < 20 {
        return Err(SyscityError::Validation("decrypted message too short".to_string()));
    }
    let len = u32::from_be_bytes(plain[16..20].try_into().unwrap_or([0; 4])) as usize;
    if 20 + len > plain.len() {
        return Err(SyscityError::Validation(
            "decrypted message length overflows payload".to_string(),
        ));
    }
    let msg = plain[20..20 + len].to_vec();
    let appid = std::str::from_utf8(&plain[20 + len..])
        .map_err(|e| SyscityError::Validation(format!("invalid appid utf8: {e}")))?
        .to_string();
    Ok((msg, appid))
}

/// AES-256-CBC encrypt `reply_xml` into the base64 `Encrypt` field.
pub fn encrypt_reply(key: &[u8; 32], reply_xml: &str, appid: &str) -> crate::Result<String> {
    let mut rng = rand::rng();
    let mut random = [0u8; 16];
    rng.fill_bytes(&mut random);

    let xml = reply_xml.as_bytes();
    let mut plain = Vec::with_capacity(16 + 4 + xml.len() + appid.len());
    plain.extend_from_slice(&random);
    plain.extend_from_slice(&(xml.len() as u32).to_be_bytes());
    plain.extend_from_slice(xml);
    plain.extend_from_slice(appid.as_bytes());

    // PKCS#7 pads to a block boundary; encrypt buffer-to-buffer.
    let mut buf = vec![0u8; plain.len() + 16];
    let ciphertext = Aes256CbcEnc::new(key.into(), key[..16].into())
        .encrypt_padded_b2b_mut::<Pkcs7>(&plain, &mut buf)
        .map_err(|_| SyscityError::Validation("AES encrypt failed".to_string()))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(ciphertext))
}

// ─────────────────────────────────────────────
// Incoming message parsing (XML)
// ─────────────────────────────────────────────

/// Parsed incoming message from the (decrypted) XML body.
#[derive(Debug, Clone, Default)]
pub struct WechatIncoming {
    pub msg_type: String,
    pub from_user: String,
    pub content: String,
    /// `event` type event (e.g. "subscribe").
    pub event: String,
}

impl WechatIncoming {
    /// True when the user text should be routed to an agent (text messages,
    /// excluding non-message events like subscribe).
    pub fn is_user_text(&self) -> bool {
        self.msg_type == "text" && !self.from_user.is_empty()
    }
}

/// Parse WeChat incoming XML (`<xml><ToUserName>...`). Uses the inner
/// (decrypted) document; element text may be wrapped in CDATA.
pub fn parse_incoming_xml(body: &str) -> crate::Result<WechatIncoming> {
    let doc = roxmltree::Document::parse(body)
        .map_err(|e| SyscityError::Validation(format!("invalid WeChat XML: {e}")))?;
    let root = doc.root_element();
    let field = |name: &str| -> String {
        root.children()
            .find(|n| n.is_element() && n.tag_name().name() == name)
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string()
    };
    Ok(WechatIncoming {
        msg_type: field("MsgType"),
        from_user: field("FromUserName"),
        content: field("Content"),
        event: field("Event"),
    })
}

/// Parse the outer webhook envelope (`<xml><Encrypt>...<MsgSignature>...`).
#[derive(Debug, Default)]
pub struct WechatEnvelope {
    pub encrypt: String,
    pub msg_signature: String,
    pub timestamp: String,
    pub nonce: String,
}

pub fn parse_envelope(body: &str) -> crate::Result<WechatEnvelope> {
    let doc = roxmltree::Document::parse(body)
        .map_err(|e| SyscityError::Validation(format!("invalid WeChat envelope XML: {e}")))?;
    let root = doc.root_element();
    let field = |name: &str| -> String {
        root.children()
            .find(|n| n.is_element() && n.tag_name().name() == name)
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string()
    };
    Ok(WechatEnvelope {
        encrypt: field("Encrypt"),
        msg_signature: field("MsgSignature"),
        timestamp: field("TimeStamp"),
        nonce: field("Nonce"),
    })
}

// ─────────────────────────────────────────────
// Channel
// ─────────────────────────────────────────────

/// Outbound adapter: delivers replies via the customer-service message API.
pub struct WechatMpChannel {
    config: WechatMpConfig,
    http_client: reqwest::Client,
    running: AtomicBool,
    access_token: RwLock<Option<(String, Instant)>>,
}

impl WechatMpChannel {
    pub fn new(config: WechatMpConfig) -> Self {
        Self {
            config,
            http_client: reqwest::Client::new(),
            running: AtomicBool::new(false),
            access_token: RwLock::new(None),
        }
    }

    /// Fetch (and cache) a valid access_token.
    async fn get_access_token(&self) -> crate::Result<String> {
        {
            let cached = self.access_token.read().await;
            if let Some((token, expires)) = cached.as_ref() {
                if expires.elapsed()
                    < Duration::from_secs(TOKEN_EXPIRY_SECS - TOKEN_REFRESH_BUFFER_SECS)
                {
                    return Ok(token.clone());
                }
            }
        }

        let url = format!(
            "{}/cgi-bin/token?grant_type=client_credential&appid={}&secret={}",
            WECHAT_API_BASE, self.config.app_id, self.config.app_secret
        );
        let resp: serde_json::Value = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("WeChat token request failed: {e}"),
                cause: Some(Box::new(e)),
            })?
            .json()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("WeChat token response parse failed: {e}"),
                cause: Some(Box::new(e)),
            })?;

        let token = resp
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SyscityError::ExternalService {
                source: format!("WeChat token error: {resp}"),
                cause: None,
            })?
            .to_string();
        *self.access_token.write().await = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    /// Send a customer-service text message to a WeChat user (openid).
    async fn send_custom_text(&self, openid: &str, content: &str) -> crate::Result<()> {
        let token = self.get_access_token().await?;
        let url = format!("{}/cgi-bin/message/custom/send?access_token={token}", WECHAT_API_BASE);
        let payload = serde_json::json!({
            "touser": openid,
            "msgtype": "text",
            "text": { "content": content },
        });
        let resp: serde_json::Value = self
            .http_client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("WeChat custom message request failed: {e}"),
                cause: Some(Box::new(e)),
            })?
            .json()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("WeChat custom message parse failed: {e}"),
                cause: Some(Box::new(e)),
            })?;

        let errcode = resp.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            return Err(SyscityError::ExternalService {
                source: format!("WeChat custom message error: {resp}"),
                cause: None,
            });
        }
        Ok(())
    }
}

#[async_trait]
impl Channel for WechatMpChannel {
    fn name(&self) -> &str {
        "wechatmp"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct],
            supports_formatting: false,
            supports_attachments: false,
            supports_images: false,
            supports_threads: false,
            supports_typing: false,
            supports_buttons: false,
            supports_commands: true,
            supports_reactions: false,
            supports_edit: false,
            supports_unsend: false,
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        info!("Starting WeChat MP channel...");
        // Warm the token; inbound is delivered via webhook regardless.
        match self.get_access_token().await {
            Ok(_) => info!("WeChat MP access_token obtained"),
            Err(e) => warn!("Failed to obtain WeChat access_token: {e}"),
        }
        self.running.store(true, Ordering::SeqCst);
        info!("WeChat MP channel started");
        info!("Configure webhook at: https://mp.weixin.qq.com (URL + token + EncodingAESKey)");
        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping WeChat MP channel...");
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let openid = &message.conversation_id.0;
        let content = match &message.formatted_content {
            Some(crate::channels::FormattedContent::Markdown(md)) => md.clone(),
            Some(crate::channels::FormattedContent::Html(html)) => html.clone(),
            _ => message.content,
        };
        self.send_custom_text(openid, &content).await?;
        debug!("WeChat MP message sent to {openid}");
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        Ok(()) // WeChat MP has no typing indicator.
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(SyscityError::Unsupported("WeChat MP does not support editing messages".into()))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(SyscityError::Unsupported("WeChat MP does not support deleting messages".into()))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.get_access_token().await.is_ok())
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // WeChat's documented test vector (EncodingAESKey / appid / plaintext).
    const AES_KEY_B64: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
    const APP_ID: &str = "wx5823bf96d3bd56c7";

    fn key() -> [u8; 32] {
        aes_key_from_encoding_key(AES_KEY_B64).unwrap()
    }

    #[test]
    fn encoding_key_decodes_to_32_bytes() {
        let k = key();
        assert_eq!(k.len(), 32);
    }

    #[test]
    fn encrypt_then_decrypt_roundtrip() {
        let k = key();
        let reply = "<xml><MsgType>text</MsgType><Content>你好</Content></xml>";
        let encrypted = encrypt_reply(&k, reply, APP_ID).unwrap();
        let plain = decrypt_message(&k, &encrypted).unwrap();
        let (msg, appid) = parse_plaintext(&plain).unwrap();
        assert_eq!(String::from_utf8(msg).unwrap(), reply);
        assert_eq!(appid, APP_ID);
    }

    #[test]
    fn signature_is_wechat_sha1() {
        // token/timestamp/nonce/encrypt sorted lexicographically, joined, sha1.
        let sig = compute_signature("token", "1400000000", "nonce", "encrypt");
        // Computed independently.
        let mut parts = ["token", "1400000000", "nonce", "encrypt"];
        parts.sort_unstable();
        let mut hasher = Sha1::new();
        hasher.update(parts.join("").as_bytes());
        assert_eq!(sig, hex::encode(hasher.finalize()));
        assert_eq!(sig.len(), 40);
    }

    #[test]
    fn echo_signature_uses_three_parts() {
        assert!(verify_echo_signature("token", "1400000000", "nonce", &{
            let mut p = ["token", "1400000000", "nonce"];
            p.sort_unstable();
            let mut h = Sha1::new();
            h.update(p.join("").as_bytes());
            hex::encode(h.finalize())
        }));
        assert!(!verify_echo_signature("token", "1400000000", "nonce", "wrong"));
    }

    #[test]
    fn parse_incoming_xml_extracts_fields_with_cdata() {
        let xml = r#"<xml>
          <ToUserName><![CDATA[gh_abc]]></ToUserName>
          <FromUserName><![CDATA[openid_123]]></FromUserName>
          <CreateTime>1700000000</CreateTime>
          <MsgType><![CDATA[text]]></MsgType>
          <Content><![CDATA[hello world]]></Content>
        </xml>"#;
        let msg = parse_incoming_xml(xml).unwrap();
        assert_eq!(msg.from_user, "openid_123");
        assert_eq!(msg.msg_type, "text");
        assert_eq!(msg.content, "hello world");
        assert!(msg.is_user_text());
    }

    #[test]
    fn parse_incoming_event() {
        let xml = r#"<xml><MsgType><![CDATA[event]]></MsgType>
          <FromUserName><![CDATA[openid_x]]></FromUserName>
          <Event><![CDATA[subscribe]]></Event></xml>"#;
        let msg = parse_incoming_xml(xml).unwrap();
        assert_eq!(msg.msg_type, "event");
        assert_eq!(msg.event, "subscribe");
        assert!(!msg.is_user_text());
    }

    #[test]
    fn parse_envelope_extracts_encrypt_and_signature() {
        let xml = r#"<xml>
          <Encrypt><![CDATA[ENCRYPTEDBLOB]]></Encrypt>
          <MsgSignature><![CDATA[SIG]]></MsgSignature>
          <TimeStamp>1400000000</TimeStamp>
          <Nonce><![CDATA[NONCE]]></Nonce>
        </xml>"#;
        let env = parse_envelope(xml).unwrap();
        assert_eq!(env.encrypt, "ENCRYPTEDBLOB");
        assert_eq!(env.msg_signature, "SIG");
        assert_eq!(env.timestamp, "1400000000");
        assert_eq!(env.nonce, "NONCE");
    }

    #[test]
    fn parse_plaintext_rejects_short_payload() {
        assert!(parse_plaintext(&[0u8; 10]).is_err());
    }
}
