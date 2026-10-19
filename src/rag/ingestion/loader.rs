//! Source loaders for Knowledge Base ingestion.
//!
//! Loads documents from file, directory, and glob sources. Supports relative
//! path resolution (relative to the agent directory) and absolute paths.

use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use tokio::fs;
use tracing::warn;

use crate::rag::chunk::ChunkStrategy;

/// A source definition loaded from `kb.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct KnowledgeSource {
    /// Unique ID within an agent (auto-generated if not specified).
    #[serde(default)]
    pub id: Option<String>,
    /// Human-readable name for this source (used in reports).
    #[serde(default)]
    pub name: String,
    /// Source type-specific configuration.
    #[serde(flatten)]
    pub source_type: SourceType,
    /// Optional glob pattern for file/dir sources (e.g. `*.md`).
    /// At the source level, applied to all file/dir loads.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Optional collection override (default: `kb-{agent_id}`).
    #[serde(default)]
    pub collection: Option<String>,
    /// Chunk strategy override (default: agent-level or global default).
    #[serde(default)]
    pub chunk_strategy: Option<ChunkStrategy>,
}

/// Supported source types.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum SourceType {
    /// Single file.
    #[serde(rename = "file")]
    File {
        /// Path to the file (relative to agent dir, or absolute).
        path: String,
    },
    /// Directory of files (optionally filtered by glob pattern).
    #[serde(rename = "dir")]
    Dir {
        /// Path to the directory.
        path: String,
    },
    /// Glob pattern relative to agent dir (recursive match).
    #[serde(rename = "glob")]
    Glob {
        /// Glob pattern (e.g. `**/*.md`).
        pattern: String,
    },
    /// Remote URL (HTTP/HTTPS). Fetched and converted to Markdown.
    #[serde(rename = "url")]
    Url {
        /// The URL to fetch (e.g. `https://example.com/docs`).
        url: String,
    },
}

/// A loaded document ready for chunking and embedding.
#[derive(Debug, Clone)]
pub struct KnowledgeDocument {
    /// Unique document identifier within the collection (filename stem).
    pub doc_id: String,
    /// Source identifier (usually the relative/original path).
    pub source_id: String,
    /// Full text content.
    pub content: String,
    /// SHA-256 checksum of the raw content.
    pub checksum: String,
    /// Last modification time (Unix timestamp in seconds), if available.
    pub mtime: Option<i64>,
    /// Detected MIME type.
    pub mime_type: String,
}

/// Load documents from a knowledge source, resolving relative paths against
/// the given agent directory.
pub async fn load_source(
    source: &KnowledgeSource,
    agent_dir: &Path,
) -> Result<Vec<KnowledgeDocument>, String> {
    match &source.source_type {
        SourceType::File { path } => {
            let full_path = resolve_path(path, agent_dir);
            let doc = load_file(&full_path).await?;
            Ok(vec![doc])
        }
        SourceType::Dir { path } => {
            let full_path = resolve_path(path, agent_dir);
            load_dir(&full_path, source.pattern.as_deref()).await
        }
        SourceType::Glob { pattern } => {
            let agent_docs = load_dir(agent_dir, None).await?;
            let filtered: Vec<KnowledgeDocument> = agent_docs
                .into_iter()
                .filter(|doc| {
                    let path = std::path::Path::new(&doc.source_id);
                    let relative = path.strip_prefix(agent_dir).unwrap_or(path);
                    glob_match(pattern, &relative.to_string_lossy())
                })
                .collect();
            Ok(filtered)
        }
        SourceType::Url { url } => {
            let doc = load_url(url).await?;
            Ok(vec![doc])
        }
    }
}

/// Load a single file as a `KnowledgeDocument`.
pub async fn load_file(path: &Path) -> Result<KnowledgeDocument, String> {
    let content = fs::read(path)
        .await
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let checksum = compute_checksum(&content);
    let mime_type = detect_mime(path);
    let mtime = fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });

    let text = if mime_type == "application/pdf" {
        extract_pdf_text(path)?
    } else if mime_type == "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    {
        extract_docx_text(path)?
    } else if mime_type == "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" {
        extract_xlsx_text(path)?
    } else {
        String::from_utf8_lossy(&content).to_string()
    };
    let doc_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("doc_{}", content.len()));

    Ok(KnowledgeDocument {
        doc_id,
        source_id: path.to_string_lossy().to_string(),
        content: text,
        checksum,
        mtime,
        mime_type: mime_type.to_string(),
    })
}

/// Load all matching files from a directory, optionally filtered by glob.
pub async fn load_dir(
    path: &Path,
    pattern: Option<&str>,
) -> Result<Vec<KnowledgeDocument>, String> {
    if !path.is_dir() {
        return Err(format!("Not a directory: {}", path.display()));
    }

    let mut docs = Vec::new();

    let mut entries = match fs::read_dir(path).await {
        Ok(r) => r,
        Err(e) => return Err(format!("Failed to read dir {}: {}", path.display(), e)),
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                warn!("Dir entry error in {}: {}", path.display(), e);
                continue;
            }
        };
        let entry_path = entry.path();

        if !entry_path.is_file() {
            continue;
        }

        // Apply glob filter if present
        if let Some(pat) = pattern {
            let filename = entry_path
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            if !glob_match(pat, &filename) {
                continue;
            }
        }

        match load_file(&entry_path).await {
            Ok(doc) => docs.push(doc),
            Err(e) => {
                // Log but continue — don't fail the whole batch for one file
                warn!("Skipping {}: {}", entry_path.display(), e);
            }
        }
    }

    Ok(docs)
}

/// Fetch a URL and convert HTML to Markdown.
async fn load_url(url: &str) -> Result<KnowledgeDocument, String> {
    let response = reqwest::get(url)
        .await
        .map_err(|e| format!("Failed to fetch URL '{}': {}", url, e))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("URL '{}' returned HTTP {}", url, status));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response from '{}': {}", url, e))?;

    let is_html = content_type
        .as_deref()
        .map(|ct| ct.contains("text/html") || ct.contains("application/xhtml"))
        .unwrap_or(false);

    let text = if is_html {
        let html = String::from_utf8_lossy(&bytes);
        crate::rag::ingestion::html_convert::html_to_markdown(&html)
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };

    let checksum = compute_checksum(bytes.as_ref());

    let doc_id = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("index")
        .to_string();

    Ok(KnowledgeDocument {
        doc_id,
        source_id: url.to_string(),
        content: text,
        checksum,
        mtime: None,
        mime_type: content_type.unwrap_or_else(|| "text/plain".to_string()),
    })
}

/// Extract text from a PDF file using pdf-extract.
fn extract_pdf_text(path: &Path) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Failed to read PDF {}: {}", path.display(), e))?;
    let text = pdf_extract::extract_text_from_mem(&bytes)
        .map_err(|e| format!("Failed to extract PDF text from {}: {}", path.display(), e))?;
    Ok(text)
}

/// Extract text from a .docx file (Word document).
///
/// .docx is a ZIP archive containing `word/document.xml`. Text is inside
/// `<w:t>` XML elements.
fn extract_docx_text(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("Failed to open docx {}: {}", path.display(), e))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("Failed to read docx archive {}: {}", path.display(), e))?;
    let mut xml = archive
        .by_name("word/document.xml")
        .map_err(|_| format!("Missing word/document.xml in docx {}", path.display()))?;
    let mut content = String::new();
    xml.read_to_string(&mut content)
        .map_err(|e| format!("Failed to read docx XML {}: {}", path.display(), e))?;
    Ok(extract_xml_text(&content))
}

/// Extract text from a .xlsx file (Excel spreadsheet).
///
/// .xlsx is a ZIP archive containing `xl/sharedStrings.xml`. Text is inside
/// `<t>` XML elements (wrapped in `<si>` items). Extracting shared strings
/// captures all text content; cell structure is not needed for KB ingestion.
fn extract_xlsx_text(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("Failed to open xlsx {}: {}", path.display(), e))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("Failed to read xlsx archive {}: {}", path.display(), e))?;
    let mut xml = archive.by_name("xl/sharedStrings.xml").map_err(|_| {
        format!(
            "Missing xl/sharedStrings.xml in xlsx {} (spreadsheet may be empty)",
            path.display()
        )
    })?;
    let mut content = String::new();
    xml.read_to_string(&mut content)
        .map_err(|e| format!("Failed to read xlsx XML {}: {}", path.display(), e))?;
    Ok(extract_xml_text(&content))
}

/// Extract text content from an OOXML XML fragment by stripping all tags.
///
/// Works for both `.docx` (`<w:t>text</w:t>`) and `.xlsx` (`<t>text</t>`)
/// formats. Handles CDATA sections and entity decoding.
fn extract_xml_text(xml: &str) -> String {
    let mut result = String::with_capacity(xml.len());
    let mut chars = xml.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '<' if chars.peek() == Some(&'!') || chars.peek() == Some(&'?') => {
                // Skip comments (<!---->), CDATA (<![CDATA[), and processing instructions (<? ?>)
                if chars.peek() == Some(&'!') {
                    // Could be comment or CDATA — consume until '>'
                    for c in chars.by_ref() {
                        if c == '>' {
                            break;
                        }
                    }
                } else {
                    // <? ... ?> — consume until '?>'
                    let mut prev = '\0';
                    for c in chars.by_ref() {
                        if prev == '?' && c == '>' {
                            break;
                        }
                        prev = c;
                    }
                }
            }
            '<' => {
                // Opening or closing tag — skip everything until '>'
                for c in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                }
            }
            '&' => {
                // Decode common XML entities
                let mut entity = String::new();
                for c in chars.by_ref() {
                    if c == ';' {
                        break;
                    }
                    entity.push(c);
                }
                let decoded = match entity.as_str() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => "", // skip unknown entities
                };
                result.push_str(decoded);
            }
            c => {
                result.push(c);
            }
        }
    }

    // Collapse multiple whitespace into single space and trim
    let mut cleaned = String::with_capacity(result.len());
    let mut prev_space = false;
    for c in result.chars() {
        if c.is_whitespace() {
            if !prev_space {
                cleaned.push(' ');
                prev_space = true;
            }
        } else {
            cleaned.push(c);
            prev_space = false;
        }
    }

    cleaned.trim().to_string()
}

/// Compute a fast SHA-256 checksum from first 4 KB + file length.
pub fn compute_checksum(content: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let prefix = content.len().min(4096);
    hasher.update(&content[..prefix]);
    hasher.update(content.len().to_le_bytes());
    hex::encode(hasher.finalize())
}

/// Detect MIME type from file extension.
pub fn detect_mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "md" | "markdown" => "text/markdown",
        "txt" => "text/plain",
        "json" => "application/json",
        "yaml" | "yml" => "application/x-yaml",
        "toml" => "application/toml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "ts" => "text/typescript",
        "py" => "text/x-python",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "java" => "text/x-java",
        "rb" => "text/x-ruby",
        "sh" => "text/x-shellscript",
        "sql" => "text/x-sql",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

/// Simple glob match (supports `*` and `?` wildcards).
pub(crate) fn glob_match(pattern: &str, filename: &str) -> bool {
    let regex_pattern = pattern_to_regex(pattern);
    // Simple regex matching without external crate
    simple_regex_match(&regex_pattern, filename)
}

/// Convert a simple glob pattern to a regex pattern string.
fn pattern_to_regex(pattern: &str) -> String {
    let mut regex = String::with_capacity(pattern.len() + 2);
    regex.push('^');
    for ch in pattern.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '.' => regex.push_str("\\."),
            '+' => regex.push_str("\\+"),
            '\\' => regex.push_str("\\\\"),
            '|' => regex.push_str("\\|"),
            '(' => regex.push_str("\\("),
            ')' => regex.push_str("\\)"),
            '[' => regex.push_str("\\["),
            ']' => regex.push_str("\\]"),
            '{' => regex.push_str("\\{"),
            '}' => regex.push_str("\\}"),
            '^' => regex.push_str("\\^"),
            '$' => regex.push_str("\\$"),
            '#' => regex.push_str("\\#"),
            ' ' => regex.push_str("\\ "),
            c => regex.push(c),
        }
    }
    regex.push('$');
    regex
}

/// Minimal regex-like matching (supports `.*`, `.`, `^`, `$`, and escaped chars).
fn simple_regex_match(pattern: &str, text: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let text_chars: Vec<char> = text.chars().collect();
    simple_match(&chars, 0, &text_chars, 0)
}

fn simple_match(pattern: &[char], pi: usize, text: &[char], ti: usize) -> bool {
    if pi == pattern.len() {
        return ti == text.len();
    }

    match pattern[pi] {
        '^' => {
            // Anchor at start
            if ti == 0 {
                simple_match(pattern, pi + 1, text, ti)
            } else {
                false
            }
        }
        '$' => {
            // Anchor at end — only matches at end of text
            if pi + 1 == pattern.len() {
                ti == text.len()
            } else {
                simple_match(pattern, pi + 1, text, ti)
            }
        }
        '.' if pi + 1 < pattern.len() && pattern[pi + 1] == '*' => {
            // `.*` — match zero or more of any character
            let mut i = ti;
            loop {
                if simple_match(pattern, pi + 2, text, i) {
                    return true;
                }
                if i >= text.len() {
                    return false;
                }
                i += 1;
            }
        }
        '.' => {
            // `.` — match any single character
            if ti < text.len() {
                simple_match(pattern, pi + 1, text, ti + 1)
            } else {
                false
            }
        }
        '\\' => {
            // Escaped character — skip backslash, match next char literally
            if pi + 1 < pattern.len() && ti < text.len() && pattern[pi + 1] == text[ti] {
                simple_match(pattern, pi + 2, text, ti + 1)
            } else {
                false
            }
        }
        c => {
            if ti < text.len() && c == text[ti] {
                simple_match(pattern, pi + 1, text, ti + 1)
            } else {
                false
            }
        }
    }
}

/// Resolve a path string: if absolute, use as-is; otherwise resolve relative
/// to `agent_dir`.
fn resolve_path(path: &str, agent_dir: &Path) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        agent_dir.join(p)
    }
}

/// Load the KB configuration from an agent directory. Returns `None` if the
/// file doesn't exist.
pub fn load_kb_config(agent_dir: &Path) -> Option<Vec<KnowledgeSource>> {
    let config_path = agent_dir.join("kb.toml");
    if !config_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(config_path).ok()?;
    #[derive(Deserialize)]
    struct KbConfig {
        #[serde(default)]
        source: Vec<KnowledgeSource>,
    }
    let cfg: KbConfig = toml::from_str(&content).ok()?;
    Some(cfg.source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_checksum() {
        let content = b"hello world";
        let checksum = compute_checksum(content);
        // SHA-256 of "hello world" prefix + content.len()
        // Just verify it's a 64-char hex string
        assert_eq!(checksum.len(), 64);
        assert!(checksum.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_detect_mime() {
        assert_eq!(detect_mime(Path::new("file.md")), "text/markdown");
        assert_eq!(detect_mime(Path::new("file.txt")), "text/plain");
        assert_eq!(detect_mime(Path::new("file.rs")), "text/x-rust");
        assert_eq!(detect_mime(Path::new("file.py")), "text/x-python");
        assert_eq!(detect_mime(Path::new("unknown.xyz")), "application/octet-stream");
        assert_eq!(
            detect_mime(Path::new("file.docx")),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
        assert_eq!(
            detect_mime(Path::new("file.xlsx")),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*.md", "readme.md"));
        assert!(!glob_match("*.md", "readme.txt"));
        assert!(glob_match("doc?.txt", "doc1.txt"));
        assert!(!glob_match("doc?.txt", "doc10.txt"));
        assert!(glob_match("*.*", "main.rs"));
    }

    #[test]
    fn test_resolve_path_absolute() {
        let agent_dir = Path::new("/tmp/agent");
        let abs = resolve_path("/etc/config.toml", agent_dir);
        assert_eq!(abs, Path::new("/etc/config.toml"));
    }

    #[test]
    fn test_resolve_path_relative() {
        let agent_dir = Path::new("/tmp/agent");
        let rel = resolve_path("kb/docs", agent_dir);
        assert_eq!(rel, Path::new("/tmp/agent/kb/docs"));
    }

    #[tokio::test]
    async fn test_load_file_not_found() {
        let result = load_file(Path::new("/nonexistent/file.md")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_load_dir_not_found() {
        let result = load_dir(Path::new("/nonexistent"), None).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_pattern_to_regex() {
        let re = pattern_to_regex("*.rs");
        assert_eq!(re, r"^.*\.rs$");
    }

    #[test]
    fn test_extract_xml_text_docx() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r>
        <w:t>Hello, World!</w:t>
      </w:r>
    </w:p>
    <w:p>
      <w:r>
        <w:t>Second paragraph with &amp; entities.</w:t>
      </w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let text = extract_xml_text(xml);
        assert_eq!(text, "Hello, World! Second paragraph with & entities.");
    }

    #[test]
    fn test_extract_xml_text_xlsx() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <si><t>Cell A1</t></si>
  <si><t>Cell B1 with text</t></si>
  <si><t>Cell A2</t></si>
</sst>"#;
        let text = extract_xml_text(xml);
        assert_eq!(text, "Cell A1 Cell B1 with text Cell A2");
    }

    #[test]
    fn test_extract_xml_text_empty() {
        assert_eq!(extract_xml_text(""), "");
        assert_eq!(extract_xml_text("<root></root>"), "");
    }
}
