//! Fixture recorder — saves real API SSE responses as golden test fixtures.
//!
//! Usage: `GRODEX_RECORD_FIXTURES=1 OPENAI_API_KEY=sk-... cargo test --test record_fixtures -- --nocapture`
//! Fixtures are saved to `tests/golden/`.

use std::fs;
use std::io::Write;

/// Record a fixture by calling the real API.
/// This test is skipped unless GRODEX_RECORD_FIXTURES is set.
#[tokio::test]
async fn record_text_only() {
    record_fixture("text_only", "Say 'Hello! How can I help you today?' and nothing else.").await;
}

#[tokio::test]
async fn record_function_call() {
    record_fixture("function_call", "Use the read_file tool to read /tmp/test.txt. The arguments should be {\"path\": \"/tmp/test.txt\"}. Output ONLY the tool call, no text.").await;
}

async fn record_fixture(name: &str, _prompt: &str) {
    if std::env::var("GRODEX_RECORD_FIXTURES").is_err() {
        eprintln!("Skipping {name}: set GRODEX_RECORD_FIXTURES=1 to record");
        return;
    }

    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            eprintln!("Skipping {name}: OPENAI_API_KEY not set");
            return;
        }
    };

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "gpt-4o-mini",
        "input": [{"role": "user", "content": _prompt}],
        "stream": true,
    });

    let response = match client
        .post("https://api.openai.com/v1/responses")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Skipping {name}: request failed: {e}");
            return;
        }
    };

    let bytes = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Skipping {name}: read failed: {e}");
            return;
        }
    };

    // Parse SSE stream and save each data line as JSONL.
    let text = String::from_utf8_lossy(&bytes);
    let path = format!("tests/golden/{name}.jsonl");
    let mut file = fs::File::create(&path).unwrap();

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(data) = trimmed.strip_prefix("data: ") {
            if data != "[DONE]" {
                // Validate it's parseable JSON.
                if serde_json::from_str::<serde_json::Value>(data).is_ok() {
                    writeln!(file, "data: {data}").unwrap();
                }
            }
        }
    }

    println!("Recorded {name}: {} ({})", path, fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
}
