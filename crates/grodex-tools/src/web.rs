//! WebFetchTool — the agent's only doorway to the internet.
//!
//! Fetches an HTTP(S) URL, strips HTML down to readable text, and caps
//! the response size so a huge page cannot blow up the context. Network
//! access is gated by the sandbox profile's `network_rules` at the
//! coordinator layer (via `SandboxManager::validate_network`).

use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, Tool, ToolMetadata, ToolRuntime};
use serde::{Deserialize, Serialize};

/// Default response cap (256KB of text after HTML stripping).
const DEFAULT_MAX_BYTES: usize = 256 * 1024;
/// Hard cap — even an explicit larger `max_bytes` is clamped to this.
const HARD_CAP_BYTES: usize = 2 * 1024 * 1024;
const FETCH_TIMEOUT_SECS: u64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchArgs {
    pub url: String,
    /// Max bytes of extracted text returned to the model.
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchOutput {
    pub url: String,
    /// Final URL after redirects (differs from `url` when the server
    /// redirected; telemetry/audit should use this one).
    pub final_url: String,
    pub status: u16,
    pub content: String,
    pub content_bytes: usize,
    pub truncated: bool,
}

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        // Redirects are followed MANUALLY via a per-hop fence: each hop's
        // host is re-checked, so `http://evil.com → 302 → http://127.0.0.1`
        // cannot bypass the SSRF fence (the initial-URL check alone can).
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
            .user_agent(format!("grodex/{}", env!("CARGO_PKG_VERSION")))
            .redirect({
                let fence = |attempt: reqwest::redirect::Attempt| {
                    if attempt.status().is_redirection() {
                        let blocked = attempt
                            .url()
                            .host_str()
                            .map(is_blocked_host)
                            .unwrap_or(true);
                        if blocked {
                            return attempt.error("redirect to private/loopback address blocked");
                        }
                    }
                    attempt.follow()
                };
                reqwest::redirect::Policy::custom(fence)
            })
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

/// SSRF fence: hosts we never fetch — loopback/link-local names, the
/// loopback/link-local IPv4 ranges, and RFC1918 private ranges (a fetch
/// is an outbound operation; internal network probing is not its job).
/// Applied to the initial URL AND every redirect hop.
fn is_blocked_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host == "0.0.0.0" {
        return true;
    }
    // IPv4 literal checks (host may carry no port — reqwest strips it).
    if let Some(ip) = host.parse::<std::net::Ipv4Addr>().ok() {
        return ip.is_loopback()
            || ip.is_link_local()
            || ip.is_private()
            || ip.is_broadcast()
            || ip.is_unspecified();
    }
    // IPv6 literal: loopback, link-local (fe80::/10), ULA (fc00::/7).
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::Ipv6Addr>() {
        return ip.is_loopback() || ip.is_unspecified();
    }
    if bare.starts_with("fe8") || bare.starts_with("fe9") || bare.starts_with("fea")
        || bare.starts_with("feb") || bare.starts_with("fc") || bare.starts_with("fd")
    {
        return true;
    }
    false
}

impl Tool for WebFetchTool {
    type Args = WebFetchArgs;
    type Output = WebFetchOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "web_fetch".into(),
            display_name: "Web Fetch".into(),
            description: "Fetch a URL over HTTP(S) and return its readable text content \
                          (HTML is stripped). Use for documentation, APIs and web pages."
                .into(),
            concurrency_class: ConcurrencyClass::Parallel,
            side_effect_class: SideEffectClass::NonIdempotent,
            default_policy: grodex_core::policy::PolicyDecision::Allow,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": {"type": "string", "description": "Absolute http(s) URL to fetch"},
                "max_bytes": {"type": "integer", "description": "Max bytes of extracted text (default 262144, hard cap 2097152)"}
            }
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "final_url": {"type": "string"},
                "status": {"type": "integer"},
                "content": {"type": "string"},
                "content_bytes": {"type": "integer"},
                "truncated": {"type": "boolean"}
            }
        })
    }
}

#[async_trait::async_trait]
impl ToolRuntime for WebFetchTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: WebFetchArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let url = reqwest::Url::parse(&args.url)
            .map_err(|e| GrodexError::ToolExecution(format!("invalid url: {e}")))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(GrodexError::ToolExecution(format!(
                "unsupported scheme '{}' (only http/https)",
                url.scheme()
            )));
        }
        // SSRF fence (redirect hops are re-checked by the redirect policy).
        if let Some(host) = url.host_str() {
            if is_blocked_host(host) {
                return Err(GrodexError::ToolExecution(
                    "refusing to fetch loopback/private address".into(),
                ));
            }
        }

        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(1024, HARD_CAP_BYTES);

        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| GrodexError::ToolExecution(format!("fetch failed: {e}")))?;

        let final_url = response.url().to_string();
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(GrodexError::ToolExecution(format!(
                "HTTP {status} for {final_url}"
            )));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Cap the raw download so a giant page cannot be pulled entirely
        // before stripping. +1KB slack so we can detect truncation.
        let raw_cap = (max_bytes * 6).min(HARD_CAP_BYTES) + 1024;
        let body = response
            .bytes()
            .await
            .map_err(|e| GrodexError::ToolExecution(format!("read body failed: {e}")))?;
        if body.len() > raw_cap {
            return Err(GrodexError::ToolExecution(format!(
                "response too large ({} bytes, cap {raw_cap})",
                body.len()
            )));
        }
        let body_str = String::from_utf8_lossy(&body);

        let is_html = content_type.contains("text/html") || content_type.contains("xhtml");
        let mut content = if is_html {
            strip_html(&body_str)
        } else {
            body_str.to_string()
        };

        let original_len = content.len();
        let truncated = original_len > max_bytes;
        if truncated {
            let mut cut = max_bytes;
            while cut > 0 && !content.is_char_boundary(cut) {
                cut -= 1;
            }
            content.truncate(cut);
            content.push_str("\n\n[... 内容因超出大小上限被截断 ...]");
        }

        let out = WebFetchOutput {
            url: args.url,
            final_url,
            status,
            content_bytes: content.len(),
            content,
            truncated,
        };
        serde_json::to_value(out).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

/// Minimal HTML → text: drop script/style/noscript blocks entirely, strip
/// remaining tags, decode the handful of entities that matter, collapse
/// whitespace. Good enough for documentation pages; not a browser.
fn strip_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    let bytes = lower.as_bytes();
    let src = html;
    while i < src.len() {
        if bytes[i] == b'<' {
            // Drop script/style/noscript including their content.
            for block in ["<script", "<style", "<noscript"] {
                if lower[i..].starts_with(block) {
                    if let Some(end) = lower[i..].find(&format!("</{}>", &block[1..])) {
                        i += end + block.len() + 2;
                    }
                }
            }
            if i < src.len() && bytes[i] == b'<' {
                if let Some(end) = lower[i..].find('>') {
                    // Block-level tags become separators.
                    let tag = &lower[i + 1..i + end];
                    if tag.starts_with("br")
                        || tag == "p"
                        || tag.starts_with("p ")
                        || tag.starts_with("p>")
                        || tag.starts_with("/p")
                        || tag.starts_with("div")
                        || tag.starts_with("/div")
                        || tag.starts_with("li")
                        || tag.starts_with("/li")
                        || tag.starts_with("h1")
                        || tag.starts_with("h2")
                        || tag.starts_with("h3")
                        || tag.starts_with("h4")
                        || tag.starts_with("/h")
                        || tag.starts_with("tr")
                        || tag.starts_with("pre")
                        || tag.starts_with("/pre")
                    {
                        out.push('\n');
                    }
                    i += end + 1;
                    continue;
                }
            }
        }
        // Copy the next char (respect char boundaries).
        let ch_len = src[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&src[i..i + ch_len]);
        i += ch_len;
    }
    // Entities.
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    // Collapse whitespace runs.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_ws = false;
    for ch in out.chars() {
        let ws = ch.is_whitespace();
        if ws && prev_ws {
            continue;
        }
        collapsed.push(if ws { ' ' } else { ch });
        prev_ws = ws;
    }
    collapsed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_scripts() {
        let html = r##"<html><head><style>body{color:red}</style></head>
            <body><h1>Title</h1><p>Hello &amp; welcome</p>
            <script>alert('x')</script><a href="#">link</a></body></html>"##;
        let text = strip_html(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn metadata_is_external() {
        let t = WebFetchTool::new();
        assert_eq!(t.metadata().side_effect_class, SideEffectClass::NonIdempotent);
        assert!(t.input_schema()["required"].as_array().unwrap().len() == 1);
    }
}
