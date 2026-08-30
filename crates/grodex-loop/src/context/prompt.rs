//! Compaction prompt — the system prompt sent to the model for summarization.
//!
//! Following Grok's 9-section structured summary format (simplified for Phase 1):
//!   1. Analysis (scratchpad, stripped from final output)
//!   2. Summary (the carry-forward context)

/// The compaction system prompt.
pub const COMPACTION_SYSTEM_PROMPT: &str = r#"You are a context compaction assistant. Your task is to summarize a conversation between an AI agent and a user into a concise, structured summary.

## Instructions

1. First, write an `<analysis>` section analyzing the conversation. Identify:
   - The user's goals and what they asked for
   - What the AI did (files read, written, edited; commands run; decisions made)
   - What worked and what didn't
   - Key files and paths mentioned
   - Any errors encountered and how they were resolved

2. Then, write a `<summary>` section that preserves ALL essential information:
   - User requests and intents
   - AI actions taken (be specific: which files, what commands)
   - Results and outcomes
   - Errors and resolutions
   - Current state (what's done, what's pending)
   - Any important context the AI will need to continue working

3. The summary must be self-contained — after compaction, only the summary will remain. The AI must be able to continue the conversation from the summary alone.

4. Keep the summary concise but complete. Prefer specifics over generalities. TARGET LENGTH: at most ~500 words (a few paragraphs plus bullets). Only exceed this if the conversation contains many distinct unfinished threads — do NOT pad, and do NOT re-narrate completed work in detail.

5. Format the summary in clear paragraphs. Use bullet points for lists of actions or files.

IMPORTANT: The `<analysis>` section will be automatically stripped before the summary is used. Only the `<summary>` section will be preserved. Do NOT put essential information only in the analysis."#;

/// Build the user prompt for compaction.
pub fn build_compaction_user_prompt(conversation_text: &str) -> String {
    format!(
        "Please summarize the following conversation:\n\n<conversation>\n{conversation_text}\n</conversation>\n\nWrite your analysis and summary now."
    )
}

/// Extract the summary from the model response.
///
/// Following Grok's `format_compact_summary`: strips the `<analysis>` scratchpad,
/// extracts the `<summary>` section, and neutralizes control tokens.
pub fn extract_summary(response: &str) -> String {
    // If there's a <summary> tag, extract its content.
    if let Some(summary_start) = response.find("<summary>") {
        let after_start = &response[summary_start + "<summary>".len()..];
        if let Some(summary_end) = after_start.find("</summary>") {
            let summary = &after_start[..summary_end];
            return clean_summary(summary);
        }
        // No closing tag — take everything after <summary>
        return clean_summary(after_start);
    }

    // No <summary> tag — use the whole response, but strip analysis if present.
    let cleaned = if let Some(analysis_end) = response.find("</analysis>") {
        response[analysis_end + "</analysis>".len()..].to_string()
    } else {
        response.to_string()
    };

    clean_summary(cleaned.trim())
}

/// Clean a summary: strip leading/trailing whitespace, prepend continuation preamble.
fn clean_summary(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "[Compaction produced no summary]".to_string();
    }
    format!(
        "This is a continuation of a previous session. Below is a summary of what happened before:\n\n---\n\n{trimmed}\n\n---\n\nContinue helping the user based on the context above."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_summary_from_response() {
        let response = "<analysis>some thinking</analysis>\n<summary>The user asked about Rust.</summary>";
        let summary = extract_summary(response);
        assert!(summary.contains("The user asked about Rust"));
        assert!(!summary.contains("some thinking"));
    }

    #[test]
    fn no_summary_tag_uses_full_response() {
        let response = "The user asked about Rust programming.";
        let summary = extract_summary(response);
        assert!(summary.contains("Rust programming"));
    }

    #[test]
    fn empty_response_produces_placeholder() {
        let summary = extract_summary("");
        assert!(summary.contains("no summary"));
    }

    #[test]
    fn summary_includes_preamble() {
        let summary = extract_summary("<summary>did things</summary>");
        assert!(summary.contains("continuation of a previous session"));
        assert!(summary.contains("did things"));
    }
}
