# Golden Fixtures

These are recorded SSE streams from real API responses.

To record a new fixture:
1. Set OPENAI_API_KEY
2. Run: `cargo test -- --nocapture record_fixtures`
3. Fixtures are saved to tests/golden/

Current fixtures:
- text_only.jsonl — recorded from OpenAI Responses API
- function_call.jsonl — recorded from OpenAI Responses API  
- multi_function_call.jsonl — recorded from OpenAI Responses API
- reasoning.jsonl — recorded from OpenAI Responses API
- stream_error.jsonl — simulated error

To verify: `cargo test --test golden_tests`
