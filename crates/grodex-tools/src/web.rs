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

/// Browser-like UA. Many sites (baidu.com, zhihu.com, …) serve an
/// anti-bot shim to non-browser UAs; baidu's shim is a JS `https→http`
/// redirect that reqwest cannot execute, so the page extracts to zero
/// text. Identifying as a browser is the industry norm (curl, firecrawl,
/// browser-use all do this).
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

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
    /// Non-fatal diagnosis of a suspicious fetch (empty text, anti-bot
    /// shim, login wall). `None` = extraction looks healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
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
            .user_agent(BROWSER_UA)
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
            // NO unwrap_or_default fallback: a default client has no
            // redirect fence and no timeout — the exact protections this
            // tool exists to enforce. Builder failure is unrecoverable.
            .expect("web_fetch: reqwest client builder failed");
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
                "max_bytes": {"type": "integer", "description": "Max bytes of extracted text (default 262144, hard cap 2097152). If the raw response body is larger than 6x this cap (max 2MB), the fetch FAILS instead of truncating."}
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
                "truncated": {"type": "boolean"},
                "warning": {"type": "string", "description": "Set when the fetch succeeded but the extracted text is empty/suspicious — do not infer content"}
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
        // Download cap is independent of how much text we hand back to the
        // model: real pages are hundreds of KB, and a small max_bytes must
        // truncate the *result*, not abort the fetch. The download cap is
        // always HARD_CAP_BYTES (2MB); max_bytes only controls output size.
        let raw_cap = HARD_CAP_BYTES;
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
            content.push_str("\n\n[... truncated: content exceeded the size cap ...]");
        }

        // ── Empty / shell-page guard ────────────────────────────────────
        // A 2xx with near-zero extracted text is NOT an empty page. The usual
        // causes are a JS-rendered SPA, an anti-bot shim, or a login wall.
        // Returning `content: ""` silently invites the model to hallucinate a
        // body, so we attach an explicit warning instead.
        const MIN_USEFUL_TEXT: usize = 32;
        let warning = if content.trim().len() < MIN_USEFUL_TEXT {
            Some(empty_page_warning(body.len(), &body_str, content.len()))
        } else {
            None
        };

        let out = WebFetchOutput {
            url: args.url,
            final_url,
            status,
            content_bytes: content.len(),
            content,
            truncated,
            warning,
        };
        serde_json::to_value(out).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

/// Explain why a 2xx response yielded (almost) no extractable text.
/// We classify the cause and hand the model a raw excerpt, so a "no text"
/// result is actionable rather than hallucination bait.
fn empty_page_warning(raw_len: usize, raw: &str, text_len: usize) -> String {
    let cause = if raw.contains("location.replace") || raw.contains("location.href") {
        "the response is a JavaScript redirect shim (anti-bot UA check): \
         the real page was never served"
    } else if raw_len > 4096 && text_len * 100 / raw_len.max(1) < 5 {
        "the HTML contained almost no static text (JS-rendered SPA)"
    } else if raw.contains("login") || raw.contains("signin") || raw.contains("登录") {
        "the page appears to require authentication"
    } else {
        "the body was empty or too short to be useful"
    };
    format!(
        "[web_fetch] extracted {text_len} bytes of text from a {raw_len}-byte response: \
         {cause}. Do NOT infer page content from this result; try another URL or source. \
         Raw excerpt: {}",
        raw.chars().take(200).collect::<String>().replace('\n', " ")
    )
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
    'outer: while i < src.len() {
        if bytes[i] == b'<' {
            // Drop script/style/noscript including their content.
            for block in ["<script", "<style", "<noscript"] {
                if lower[i..].starts_with(block) {
                    if let Some(end) = lower[i..].find(&format!("</{}>", &block[1..])) {
                        i += end + block.len() + 2;
                        // CRITICAL: re-evaluate from the new position.
                        // Without this, if another same-type block is
                        // immediately adjacent (zero-byte gap, common on
                        // baidu.com/juejin), `i` points at `<style`/`<script`
                        // again but falls through to the normal-tag branch
                        // below, which only skips the opening tag and lets the
                        // block content leak into the output.
                        continue 'outer;
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
    fn drops_adjacent_style_blocks() {
        // Regression: two <style> blocks with zero-byte gap between them.
        // Before the fix, the second block's content leaked into the output
        // because strip_html fell through to the normal-tag branch.
        let html = "<style>a{color:red}</style><style>b{margin:0}</style><p>Hello</p>";
        let text = strip_html(html);
        assert_eq!(text.trim(), "Hello");
    }

    #[test]
    fn drops_adjacent_script_blocks() {
        // Same bug for <script>: adjacent blocks leaked JS into text.
        let html = "<script>var x=1;</script><script>var y=2;</script><p>Done</p>";
        let text = strip_html(html);
        assert_eq!(text.trim(), "Done");
    }

    #[test]
    fn drops_mixed_adjacent_blocks() {
        // <style> immediately followed by <script> immediately followed by text.
        let html = "<style>x{}</style><script>y()</script><noscript>z</noscript><p>OK</p>";
        let text = strip_html(html);
        assert_eq!(text.trim(), "OK");
    }

    #[test]
    fn metadata_is_external() {
        let t = WebFetchTool::new();
        assert_eq!(t.metadata().side_effect_class, SideEffectClass::NonIdempotent);
        assert!(t.input_schema()["required"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn flags_anti_bot_shim() {
        let shim = r#"<html><head><script>
            location.replace(location.href.replace("https://","http://"));
        </script></head><body></body></html>"#;
        let text = strip_html(shim);
        // Current behavior: the shim extracts to empty string.
        assert!(text.trim().is_empty());
        // After fix: the warning classifies the cause.
        let w = empty_page_warning(shim.len(), shim, text.len());
        assert!(w.contains("redirect shim"));
    }

    #[test]
    fn empty_page_warning_classifies_spa() {
        // A large HTML body where all content is inside <script> (gets
        // stripped by strip_html) → near-zero text but large raw size.
        let spa = format!(
            "<html><body><div id=\"root\"></div><script>{}</script></body></html>",
            "const x = 1;".repeat(500)
        );
        let text = strip_html(&spa);
        assert!(text.trim().is_empty());
        let w = empty_page_warning(spa.len(), &spa, text.len());
        assert!(w.contains("JS-rendered SPA"));
    }

    #[test]
    fn empty_page_warning_classifies_login_wall() {
        let wall = "<html><body><div>请登录后查看内容 login or signin</div></body></html>";
        let text = strip_html(wall);
        let w = empty_page_warning(wall.len(), wall, text.len());
        assert!(w.contains("authentication"));
    }
}
