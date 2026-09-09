//! Remote connector catalog — fetch, integrity-check, cache, and install
//! entries from a hosted marketplace directory.
//!
//! A catalog is a JSON document listing published connector packages:
//!
//! ```json
//! {
//!   "version": 1,
//!   "connectors": [
//!     {
//!       "id": "linear-mcp", "version": "1.2.0",
//!       "display_name": "Linear", "description": "Issue tracking",
//!       "icon": "icons/linear.svg",
//!       "source": { "type": "tar.gz", "url": "https://example.com/l.tar.gz" },
//!       "sha256": "<hex digest of the archive>",
//!       "auto_update": true
//!     }
//!   ]
//! }
//! ```
//!
//! Sync uses HTTP conditional requests: an `ETag` from the previous sync is
//! sent as `If-None-Match`, and a `304 Not Modified` short-circuits to the
//! cached copy. Fresh bodies are hashed (`sha256`) and stored alongside so a
//! later consumer can detect local tampering before trusting the catalog.
//!
//! Entry install downloads the archive, verifies it against the entry's
//! `sha256` when present, extracts into the versioned install cache
//! (`cache/<id>/<version>/`), and returns the package root (the archive may
//! wrap everything in one top-level folder).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::plugins::installer::{locate_package_root, PluginInstaller};

// ─────────────────────────────────────────────
// Catalog document types
// ─────────────────────────────────────────────

/// Root of a remote catalog document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogDocument {
    /// Catalog schema version.
    #[serde(default = "default_catalog_version")]
    pub version: u32,
    /// Published connector entries.
    #[serde(default)]
    pub connectors: Vec<CatalogEntry>,
}

fn default_catalog_version() -> u32 {
    1
}

/// One published marketplace entry (connector / skill / expert).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Connector id (must match the package manifest id).
    pub id: String,
    /// Package version (becomes the cache directory name).
    pub version: String,
    /// Display name (informational; the manifest is authoritative).
    #[serde(default)]
    pub display_name: String,
    /// Short description.
    #[serde(default)]
    pub description: String,
    /// Icon URL or catalog-relative path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Entry type (`"connector"` | `"skill"` | `"expert"`, default connector).
    #[serde(default = "default_entry_type", rename = "type")]
    pub entry_type: String,
    /// Distribution kind (`"byoa"` | `"cloud"`, default byoa): cloud entries
    /// are cloud-provisioned and routed through the cloud MCP relay.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Visibility (`"public"` | `"member"`): member entries are login-only.
    #[serde(default = "default_visibility")]
    pub visibility: String,
    /// Per-call credit cost for cloud entries (0 for byoa / skills).
    #[serde(default)]
    pub credits_per_use: i64,
    /// Single-value capability category for marketplace browse/filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Optional starter prompt for experts/skills: pre-filled into the chat
    /// composer when the entry is summoned / "tried".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starter_prompt: Option<String>,
    /// Where the package archive lives.
    pub source: CatalogSource,
    /// Expected sha256 (lowercase hex) of the downloaded archive. When set,
    /// mismatches are a hard error; when absent the download is trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Whether `apply_updates(auto_only=true)` may upgrade this connector
    /// without an explicit per-entry confirmation.
    #[serde(default)]
    pub auto_update: bool,
}

fn default_entry_type() -> String {
    "connector".to_string()
}

fn default_kind() -> String {
    "byoa".to_string()
}

fn default_visibility() -> String {
    "public".to_string()
}

/// Archive location and format for a catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSource {
    /// Archive format: `"tar.gz"` (also accepts `"tgz"`) or `"zip"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Absolute download URL.
    pub url: String,
}

// ─────────────────────────────────────────────
// Sync metadata + pure decision logic
// ─────────────────────────────────────────────

/// Persisted sync bookkeeping (`catalog/meta.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogMeta {
    /// ETag from the last successful response, if the server sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// sha256 (hex) of the last accepted catalog body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Last successful sync time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<DateTime<Utc>>,
    /// Language bucket (`"en"` | `"zh"`) of the last accepted body. The
    /// single-slot cache holds one language at a time; a request for a
    /// different bucket forces a full refresh instead of a 304.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

/// Normalize a request language to the catalog's translation buckets —
/// mirrors the cloud's `resolveLang`: `zh*` → `"zh"`, everything else
/// (including no preference) → `"en"`.
pub(crate) fn catalog_lang_bucket(lang: Option<&str>) -> &'static str {
    match lang {
        Some(l) if l.to_lowercase().starts_with("zh") => "zh",
        _ => "en",
    }
}

/// Outcome of a conditional GET against the catalog URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchDecision {
    /// Keep the cached body (304, or a 200 whose content we already have).
    KeepCached,
    /// Replace the cache with the fresh body.
    Replace,
}

/// Pure decision for a conditional catalog response.
///
/// Unit-tested directly; the network layer only maps HTTP statuses onto this.
/// A `200` whose body hash matches the stored hash is treated as unchanged so
/// servers without ETag support still avoid pointless cache rewrites.
fn should_replace(meta: &CatalogMeta, status: u16, new_body_hash_hex: &str) -> FetchDecision {
    match status {
        304 => FetchDecision::KeepCached,
        _ => {
            if meta.sha256.as_deref() == Some(new_body_hash_hex) {
                FetchDecision::KeepCached
            } else {
                FetchDecision::Replace
            }
        }
    }
}

/// Compute lowercase-hex sha256 over `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

/// Ordering key for catalog versions (semver when parseable, else raw string).
pub(crate) fn catalog_version_key(v: &str) -> (u64, u64, u64, String) {
    if let Ok(p) = crate::skills::semver::Version::parse(v) {
        (p.major, p.minor, p.patch, v.to_string())
    } else {
        (0, 0, 0, v.to_string())
    }
}

// ─────────────────────────────────────────────
// Updates diffing
// ─────────────────────────────────────────────

/// An installed connector whose catalog version differs from the local one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpdate {
    /// Connector id.
    pub id: String,
    /// Locally installed version.
    pub current_version: String,
    /// Version offered by the remote catalog.
    pub latest_version: String,
    /// The remote entry driving the update.
    pub entry: CatalogEntry,
}

/// Diff installed `(id, version)` pairs against catalog entries.
///
/// An entry is an update when the id matches, the remote version differs from
/// the installed one (unequal — catalogs may re-publish corrected archives),
/// and the entry actually has a downloadable source.
pub fn diff_updates(
    installed: impl IntoIterator<Item = (String, String)>,
    catalog: &[CatalogEntry],
) -> Vec<PendingUpdate> {
    let mut updates = Vec::new();
    for (id, current) in installed {
        if let Some(entry) = catalog
            .iter()
            .find(|e| e.id == id && e.version != current)
            .filter(|e| !e.source.url.is_empty())
        {
            updates.push(PendingUpdate {
                id: entry.id.clone(),
                current_version: current,
                latest_version: entry.version.clone(),
                entry: entry.clone(),
            });
        }
    }
    updates
}

// ─────────────────────────────────────────────
// Network-backed operations
// ─────────────────────────────────────────────

/// Cache layout rooted at `<connectors_dir>/catalog/`.
pub struct CatalogCache {
    dir: PathBuf,
    client: reqwest::Client,
}

impl CatalogCache {
    /// Cache under `dir` (`catalog.json` + `meta.json` live here).
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            client: reqwest::Client::new(),
        }
    }

    fn body_path(&self) -> PathBuf {
        self.dir.join("catalog.json")
    }

    fn meta_path(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    async fn load_meta(&self) -> CatalogMeta {
        match tokio::fs::read_to_string(self.meta_path()).await {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => CatalogMeta::default(),
        }
    }

    async fn write_meta(&self, meta: &CatalogMeta) -> crate::Result<()> {
        let bytes = serde_json::to_vec_pretty(meta).unwrap_or_else(|_| Vec::new());
        tokio::fs::write(self.meta_path(), bytes).await?;
        Ok(())
    }

    /// Load the last synced catalog, if any.
    pub async fn cached(&self) -> crate::Result<Option<CatalogDocument>> {
        let raw = match tokio::fs::read_to_string(self.body_path()).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(crate::error::SyscityError::IoContext {
                    context: format!("Failed to read {}", self.body_path().display()),
                    source: e,
                })
            }
        };
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| crate::error::SyscityError::Validation(format!("cached catalog: {e}")))
    }

    /// Language bucket (`"en"` | `"zh"`) of the last synced catalog, if known.
    /// Older meta files (pre-lang) read as `None`.
    pub async fn cached_lang(&self) -> Option<String> {
        self.load_meta().await.lang
    }

    /// Fetch the remote catalog with conditional-request semantics.
    ///
    /// When `token` is present it is sent as `Authorization: Bearer` so the
    /// cloud catalog serves the full, login-visible view (anonymous sees only
    /// `public` entries — §3.6). ETags are per-view, so switching between
    /// anonymous and authed views naturally refreshes the cache.
    ///
    /// `lang` (when set) is sent as `Accept-Language` — `catalog.json` is
    /// bilingual and the cache is single-slot, so the last body's language
    /// bucket is tracked in `meta.json`: a request for a different bucket
    /// skips `If-None-Match` to force a full 200 refresh in the new language
    /// instead of a 304 against the old language's ETag.
    ///
    /// Returns the (possibly cached) document plus whether the cache was
    /// refreshed during this call.
    pub async fn sync(
        &self,
        url: &str,
        token: Option<&str>,
        lang: Option<&str>,
    ) -> crate::Result<(CatalogDocument, bool)> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let meta = self.load_meta().await;
        // The cache is single-slot and the response is per-language: when the
        // requested bucket differs from the cached one, skip If-None-Match so
        // the server answers 200 with the new language instead of 304-ing
        // against the old language's ETag.
        let want = catalog_lang_bucket(lang);
        let lang_changed = meta.lang.as_deref() != Some(want);

        let mut request = self.client.get(url);
        if !lang_changed {
            if let Some(etag) = &meta.etag {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag);
            }
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(lang) = lang {
            request = request.header(reqwest::header::ACCEPT_LANGUAGE, lang);
        }
        let response = request.send().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Catalog fetch failed: {e}"))
        })?;
        let status = response.status().as_u16();
        // Capture the header before the body consumes the response.
        let fresh_etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let decision = match status {
            304 => FetchDecision::KeepCached,
            200 => {
                let body = response.bytes().await.map_err(|e| {
                    crate::error::SyscityError::Internal(format!("Catalog read failed: {e}"))
                })?;
                let body_hash = sha256_hex(&body);
                match should_replace(&meta, status, &body_hash) {
                    FetchDecision::KeepCached => {
                        // Body unchanged but the language tracking moved
                        // (identical zh/en content): persist the new bucket so
                        // later syncs conditional-request again.
                        if lang_changed {
                            self.write_meta(&CatalogMeta {
                                lang: Some(want.to_string()),
                                ..meta.clone()
                            })
                            .await?;
                        }
                        FetchDecision::KeepCached
                    }
                    FetchDecision::Replace => {
                        let text = String::from_utf8_lossy(&body).into_owned();
                        let doc: CatalogDocument = serde_json::from_str(&text).map_err(|e| {
                            crate::error::SyscityError::Validation(format!("catalog: {e}"))
                        })?;
                        let new_meta = CatalogMeta {
                            etag: fresh_etag.or(meta.etag.clone()),
                            sha256: Some(body_hash),
                            synced_at: Some(Utc::now()),
                            lang: Some(want.to_string()),
                        };
                        tokio::fs::write(self.body_path(), text.as_bytes()).await?;
                        self.write_meta(&new_meta).await?;
                        info!(
                            "Connector catalog refreshed ({} entries) from {url}",
                            doc.connectors.len()
                        );
                        return Ok((doc, true));
                    }
                }
            }
            other => {
                return Err(crate::error::SyscityError::Internal(format!(
                    "Catalog fetch returned unexpected status {other}"
                )))
            }
        };

        match decision {
            FetchDecision::KeepCached => {
                let doc = self.cached().await?.ok_or_else(|| {
                    crate::error::SyscityError::Internal(
                        "Server answered 304 but no cached catalog exists".to_string(),
                    )
                })?;
                Ok((doc, false))
            }
            FetchDecision::Replace => unreachable!("handled inline above"),
        }
    }

    /// Download, verify, extract, and unpack one catalog entry into
    /// `cache_root/<id>/<version>/`; returns the effective package root.
    ///
    /// Idempotent: reinstalling an identical version is a no-op.
    pub async fn install_entry(
        &self,
        entry: &CatalogEntry,
        cache_root: &Path,
    ) -> crate::Result<PathBuf> {
        // Only connectors ship a connector.json manifest: skills are bare
        // SKILL.md folders and experts are SOUL.md role packages, so the
        // marker selects what counts as the package root for each type.
        let marker = match entry.entry_type.as_str() {
            "skill" => "SKILL.md",
            "expert" => "SOUL.md",
            _ => "connector.json",
        };
        let dest = cache_root.join(&entry.id).join(&entry.version);
        let already_cached = match entry.entry_type.as_str() {
            "skill" => dest.join("SKILL.md").exists(),
            "expert" => dest.join("SOUL.md").exists() || dest.join("agents").is_dir(),
            _ => dest.join("connector.json").exists(),
        };
        if already_cached {
            return Ok(locate_package_root(&dest, marker));
        }

        let response = self
            .client
            .get(&entry.source.url)
            .send()
            .await
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!(
                    "Failed to download connector {} v{}: {e}",
                    entry.id, entry.version
                ))
            })?;
        if !response.status().is_success() {
            return Err(crate::error::SyscityError::Internal(format!(
                "Download for connector {} returned {}",
                entry.id,
                response.status()
            )));
        }
        let archive = response.bytes().await?;

        // Integrity gate: hard-fail when the publisher pinned a hash.
        if let Some(expected) = &entry.sha256 {
            let actual = sha256_hex(&archive);
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Connector {} archive checksum mismatch: expected {expected}, got {actual}",
                    entry.id
                )));
            }
        }

        // Stage: download + extraction happen under a temp dir; the discovered
        // package root moves to its final home only after everything succeeded.
        let stage = cache_root.join(format!(".stage-{}-{}", entry.id, entry.version));
        let _ = tokio::fs::remove_dir_all(&stage).await;
        let unpacked = stage.join("unpacked");
        tokio::fs::create_dir_all(&unpacked).await?;

        let result: crate::Result<PathBuf> = async {
            let suffix = if entry.source.kind.eq_ignore_ascii_case("zip") {
                "zip"
            } else {
                "tar.gz"
            };
            let archive_path = stage.join(format!("package.{suffix}"));
            tokio::fs::write(&archive_path, &archive)
                .await
                .map_err(|e| crate::error::SyscityError::IoContext {
                    context: format!("Failed to buffer archive for {}", entry.id),
                    source: e,
                })?;
            PluginInstaller::extract_archive(&archive_path, &unpacked).await?;
            tokio::fs::remove_file(&archive_path).await.ok();

            // Connectors need their manifest; skill/expert packages are
            // free-form (SKILL.md folders / SOUL.md role layouts, possibly
            // multi-agent) — their installers validate the shape and error
            // with a precise message, so hand the whole unpacked tree over.
            let root = locate_package_root(&unpacked, marker);
            if entry.entry_type != "skill"
                && entry.entry_type != "expert"
                && !root.join(marker).exists()
            {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Archive for connector {} contains no connector.json",
                    entry.id
                )));
            }
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            if dest.exists() {
                tokio::fs::remove_dir_all(&dest).await?;
            }
            tokio::fs::rename(&root, &dest).await.map_err(|e| {
                crate::error::SyscityError::IoContext {
                    context: format!("Failed to finalize install of connector {}", entry.id),
                    source: e,
                }
            })?;
            Ok(dest)
        }
        .await;

        let _ = tokio::fs::remove_dir_all(&stage).await;
        let dest = result?;
        info!("Installed connector {} v{} to {}", entry.id, entry.version, dest.display());
        Ok(dest)
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::installer::extract_zip;

    fn entry(id: &str, version: &str) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            version: version.to_string(),
            display_name: id.to_string(),
            description: String::new(),
            icon: None,
            entry_type: "connector".to_string(),
            kind: "byoa".to_string(),
            visibility: "public".to_string(),
            credits_per_use: 0,
            category: None,
            starter_prompt: None,
            source: CatalogSource {
                kind: "tar.gz".to_string(),
                url: format!("https://example.com/{id}-{version}.tar.gz"),
            },
            sha256: None,
            auto_update: false,
        }
    }

    #[test]
    fn parses_catalog_document() {
        let doc: CatalogDocument = serde_json::from_str(
            r#"{
            "version": 1,
            "connectors": [{
                "id": "linear-mcp", "version": "1.2.0", "display_name": "Linear",
                "source": {"type": "tar.gz", "url": "https://x/l.tgz"},
                "sha256": "abc123", "auto_update": true
            }]
        }"#,
        )
        .unwrap();
        assert_eq!(doc.connectors.len(), 1);
        assert_eq!(doc.connectors[0].source.kind, "tar.gz");
        assert!(doc.connectors[0].auto_update);
        assert_eq!(doc.connectors[0].sha256.as_deref(), Some("abc123"));
    }

    #[test]
    fn sha256_is_stable_and_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex(b"syscity").len(), 64);
    }

    #[test]
    fn starter_prompt_parses_and_serializes_optionally() {
        // Present → parsed.
        let with: CatalogEntry = serde_json::from_str(
            r#"{
            "id": "e1", "version": "1.0.0", "type": "expert",
            "starter_prompt": "帮我规划今天的工作",
            "source": {"type": "tar.gz", "url": "https://example.com/e1.tgz"}
        }"#,
        )
        .unwrap();
        assert_eq!(with.starter_prompt.as_deref(), Some("帮我规划今天的工作"));
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains("starter_prompt"));

        // Absent → None (older catalogs keep parsing); round-trips without
        // the key (skip_serializing_if keeps the wire shape stable).
        let without: CatalogEntry = serde_json::from_str(
            r#"{
            "id": "e2", "version": "1.0.0",
            "source": {"type": "tar.gz", "url": "https://example.com/e2.tgz"}
        }"#,
        )
        .unwrap();
        assert_eq!(without.starter_prompt, None);
        let json = serde_json::to_string(&without).unwrap();
        assert!(!json.contains("starter_prompt"));
    }

    #[test]
    fn lang_bucket_mirrors_cloud_resolution() {
        assert_eq!(catalog_lang_bucket(None), "en");
        assert_eq!(catalog_lang_bucket(Some("en")), "en");
        assert_eq!(catalog_lang_bucket(Some("en-US")), "en");
        assert_eq!(catalog_lang_bucket(Some("zh")), "zh");
        assert_eq!(catalog_lang_bucket(Some("zh-CN")), "zh");
        assert_eq!(catalog_lang_bucket(Some("ZH-tw")), "zh");
        assert_eq!(catalog_lang_bucket(Some("fr")), "en");
    }

    #[test]
    fn meta_lang_round_trips() {
        let meta = CatalogMeta {
            lang: Some("zh".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: CatalogMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lang.as_deref(), Some("zh"));
        // Pre-lang meta files (no `lang` key) read as None, not an error.
        let legacy: CatalogMeta = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.lang, None);
    }

    #[tokio::test]
    async fn sync_re_syncs_when_language_changes() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let en_body = r#"{"version":1,"connectors":[{"id":"a","version":"1.0.0",
            "display_name":"Alpha","description":"English description",
            "source":{"type":"tar.gz","url":"https://example.com/a.tgz"}}]}"#;
        let zh_body = r#"{"version":1,"connectors":[{"id":"a","version":"1.0.0",
            "display_name":"阿尔法","description":"中文描述",
            "source":{"type":"tar.gz","url":"https://example.com/a.tgz"}}]}"#;

        let server = MockServer::start().await;
        let url = format!("{}/catalog.json", server.uri());
        Mock::given(method("GET"))
            .and(path("/catalog.json"))
            .and(header("accept-language", "en"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(en_body)
                    .append_header("ETag", "\"e-en\""),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/catalog.json"))
            .and(header("accept-language", "en"))
            .and(header("if-none-match", "\"e-en\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/catalog.json"))
            .and(header("accept-language", "zh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(zh_body)
                    .append_header("ETag", "\"e-zh\""),
            )
            .mount(&server)
            .await;
        // A stale (English) ETag must never leak into a zh request — the
        // language change skips If-None-Match entirely.
        Mock::given(method("GET"))
            .and(path("/catalog.json"))
            .and(header("accept-language", "zh"))
            .and(header("if-none-match", "\"e-en\""))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let cache = CatalogCache::new(tmp.path().to_path_buf());

        // First sync resolves English and records the bucket.
        let (doc, refreshed) = cache.sync(&url, None, Some("en")).await.unwrap();
        assert!(refreshed);
        assert_eq!(doc.connectors[0].display_name, "Alpha");
        assert_eq!(cache.cached_lang().await.as_deref(), Some("en"));

        // Same language → conditional request → 304 keeps the cache.
        let (_, refreshed) = cache.sync(&url, None, Some("en")).await.unwrap();
        assert!(!refreshed);

        // Language change → full refresh in Chinese (no stale If-None-Match).
        let (doc, refreshed) = cache.sync(&url, None, Some("zh")).await.unwrap();
        assert!(refreshed);
        assert_eq!(doc.connectors[0].display_name, "阿尔法");
        assert_eq!(cache.cached_lang().await.as_deref(), Some("zh"));

        // Same language again → 304 keeps the cache.
        let (_, refreshed) = cache.sync(&url, None, Some("zh")).await.unwrap();
        assert!(!refreshed);
    }

    #[test]
    fn decision_304_keeps_cache_without_a_body() {
        let meta = CatalogMeta::default();
        assert_eq!(should_replace(&meta, 304, ""), FetchDecision::KeepCached);
    }

    #[test]
    fn decision_200_with_identical_hash_keeps_cache() {
        let meta = CatalogMeta {
            sha256: Some(sha256_hex(b"same")),
            ..Default::default()
        };
        assert_eq!(should_replace(&meta, 200, &sha256_hex(b"same")), FetchDecision::KeepCached);
        assert_eq!(should_replace(&meta, 200, &sha256_hex(b"different")), FetchDecision::Replace);
    }

    #[test]
    fn decision_200_on_empty_meta_replaces() {
        assert_eq!(
            should_replace(&CatalogMeta::default(), 200, "anything"),
            FetchDecision::Replace
        );
    }

    #[test]
    fn update_diff_finds_changed_entries_only() {
        let installed = vec![
            ("a".to_string(), "1.0.0".to_string()),
            ("b".to_string(), "2.0.0".to_string()),
            ("gone".to_string(), "1.0.0".to_string()),
        ];
        let catalog = vec![
            entry("a", "1.1.0"),
            entry("b", "2.0.0"),
            entry("new", "0.1.0"),
        ];
        let updates = diff_updates(installed, &catalog);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id, "a");
        assert_eq!(updates[0].current_version, "1.0.0");
        assert_eq!(updates[0].latest_version, "1.1.0");
    }

    #[tokio::test]
    async fn zip_extraction_rejects_traversal_entries() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("out");

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("../evil.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"pwn").unwrap();
        let cursor = zip.finish().unwrap();
        let archive_path = tmp.path().join("evil.zip");
        std::fs::write(&archive_path, cursor.into_inner()).unwrap();

        let err = extract_zip(&archive_path, &target).await.unwrap_err();
        assert!(err.to_string().contains("unsafe"), "{err}");
        assert!(!target.parent().unwrap().join("evil.txt").exists());
    }

    #[tokio::test]
    async fn zip_extraction_unpacks_flat_entries() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("out");

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.add_directory("pkg", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.start_file("pkg/connector.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"{}").unwrap();
        let cursor = zip.finish().unwrap();
        let archive_path = tmp.path().join("ok.zip");
        std::fs::write(&archive_path, cursor.into_inner()).unwrap();

        extract_zip(&archive_path, &target).await.unwrap();
        assert!(target.join("pkg/connector.json").exists());
    }

    #[test]
    fn locates_wrapped_package_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("extract");
        std::fs::create_dir_all(root.join("wrapper")).unwrap();
        std::fs::write(root.join("wrapper/connector.json"), "{}").unwrap();

        let found = locate_package_root(&root, "connector.json");
        assert_eq!(found, root.join("wrapper"));
    }
}
