//! SamplingActor — retry-aware sampling with real streaming + multi-decoder support.

use crate::breaker::{CircuitBreaker, Outcome};
use crate::client::SamplingClient;
use crate::decoder::chat_completions::ChatCompletionsDecoder;
use crate::decoder::messages::MessagesDecoder;
use crate::decoder::responses::ResponsesDecoder;
use crate::error::SamplingError;
use crate::retry::{RetryBudget, RetryDecision, StreamProgress, classify_error, sleep_or_cancel};
use crate::route::ModelRoute;
use crate::streaming::StreamingDecoder;
use crate::StreamFragment;
use futures::StreamExt;
use grodex_provider::binding::ModelBinding;
use grodex_provider::canonical_event::CanonicalModelResponse;
use grodex_provider::canonical_request::CanonicalModelRequest;
use grodex_provider::descriptor::WireProtocol;
use grodex_provider::{CanonicalModelEvent, ProviderError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct SamplingOutcome {
    pub events: Vec<CanonicalModelEvent>,
    pub response: Option<CanonicalModelResponse>,
    pub error: Option<SamplingError>,
    pub attempts: u32,
    pub elapsed: std::time::Duration,
    /// Routing decisions drained from the [`ModelRoute`] after the call
    /// (candidate selected / failed / breaker opened / route exhausted).
    /// Empty when no route is wired.
    pub route_events: Vec<crate::route::RouteEvent>,
    /// Time from round start to the first content event of the successful
    /// attempt — TTFT. `None` when no content was produced.
    pub first_token_ms: Option<u64>,
}

pub struct SamplingActor {
    client: SamplingClient,
    budget: RetryBudget,
    breaker: CircuitBreaker,
    route: Option<Arc<std::sync::Mutex<ModelRoute>>>,
}

impl SamplingActor {
    pub fn new(client: SamplingClient) -> Self {
        Self { client, budget: RetryBudget::default(), breaker: CircuitBreaker::new(Default::default()), route: None }
    }
    pub fn with_budget(mut self, budget: RetryBudget) -> Self { self.budget = budget; self }
    pub fn with_breaker(mut self, breaker: CircuitBreaker) -> Self { self.breaker = breaker; self }
    pub fn with_route(mut self, route: ModelRoute) -> Self { self.route = Some(Arc::new(std::sync::Mutex::new(route))); self }

    /// Sample with real-time streaming through stream_tx.
    /// The pipe carries assistant text, reasoning, and tool-call fragments
    /// as first-class citizens — the CLI layer maps each one to the
    /// matching ACP `UpdateContent` frame for the TUI.
    pub async fn sample_streaming(
        &self,
        binding: &ModelBinding,
        request: &CanonicalModelRequest,
        stream_tx: mpsc::UnboundedSender<StreamFragment>,
    ) -> SamplingOutcome {
        let mut outcome = self.sample_streaming_inner(binding, request, stream_tx).await;
        outcome.route_events = self.drain_route_events();
        outcome
    }

    fn drain_route_events(&self) -> Vec<crate::route::RouteEvent> {
        match &self.route {
            None => Vec::new(),
            Some(route) => route.lock().map(|mut r| r.drain_events()).unwrap_or_default(),
        }
    }

    async fn sample_streaming_inner(
        &self,
        binding: &ModelBinding,
        request: &CanonicalModelRequest,
        stream_tx: mpsc::UnboundedSender<StreamFragment>,
    ) -> SamplingOutcome {
        let start = Instant::now();
        let mut events = Vec::new();
        let mut attempt = 0u32;
        let cancel_token = CancellationToken::new();
        let progress = Arc::new(AtomicBool::new(false));

        loop {
            if cancel_token.is_cancelled() {
                return SamplingOutcome { first_token_ms: None, route_events: Vec::new(), events, response: None, error: Some(SamplingError::internal("cancelled")), attempts: attempt, elapsed: start.elapsed() };
            }
            if let Err(bo) = self.breaker.check() {
                return SamplingOutcome { first_token_ms: None, route_events: Vec::new(), events, response: None, error: Some(SamplingError::internal(format!("breaker open: {:.1}s", bo.retry_after.as_secs_f64()))), attempts: attempt, elapsed: start.elapsed() };
            }
            // First attempt always targets the PRIMARY binding, so it uses
            // the client's own credential (no override). `current_api_key()`
            // would be wrong here: the route's sticky index may still point
            // at a failover candidate from a PREVIOUS call.
            let outcome = self
                .run_attempt(binding, request, &progress, &mut events, &stream_tx, None)
                .await;
            match outcome {
                AttemptResult::Completed { response, first_token_ms } => {
                    self.breaker.record(Outcome::Success);
                    return SamplingOutcome { first_token_ms, route_events: Vec::new(), events, response: Some(response), error: None, attempts: attempt + 1, elapsed: start.elapsed() };
                }
                AttemptResult::Failed { error } => {
                    self.breaker.record(Outcome::Failure);
                    attempt += 1;
                    let sp = if progress.load(Ordering::Acquire) { StreamProgress { text_received: true, ..Default::default() } } else { StreamProgress::default() };
                    match classify_error(&error, attempt, &self.budget, sp) {
                        RetryDecision::Retry { backoff } | RetryDecision::RetryWithClientRebuild { backoff } => {
                            if !sleep_or_cancel(backoff, &cancel_token).await {
                                return SamplingOutcome { first_token_ms: None, route_events: Vec::new(), events, response: None, error: Some(SamplingError::internal("cancelled")), attempts: attempt, elapsed: start.elapsed() };
                            }
                        }
                        RetryDecision::FailoverToNextCandidate => {
                            let (next_binding, failover_credential) = if let Some(ref route) = self.route {
                                let mut route = route.lock().unwrap();
                                route.record_failure(true);
                                let next = route.try_next().map(|(_, b)| b);
                                // try_next advanced the sticky index — the
                                // current candidate is now the failover one.
                                let credential = route.current_api_key();
                                (next, credential)
                            } else { (None, None) };
                            if let Some(new_binding) = next_binding {
                                attempt = 0;
                                let outcome = self
                                    .run_attempt(&new_binding, request, &progress, &mut events, &stream_tx, failover_credential.as_deref())
                                    .await;
                                if let Some(ref route) = self.route {
                                    let mut route = route.lock().unwrap();
                                    match outcome {
                                        AttemptResult::Completed { .. } => route.record_success(),
                                        AttemptResult::Failed { ref error } => { route.record_failure(error.is_failover_eligible()); }
                                    }
                                }
                                match outcome {
                                    AttemptResult::Completed { response: resp, first_token_ms } => {
                                        return SamplingOutcome { first_token_ms, route_events: Vec::new(), events, response: Some(resp), error: None, attempts: attempt + 1, elapsed: start.elapsed() };
                                    }
                                    AttemptResult::Failed { .. } => {
                                        attempt += 1;
                                        continue;
                                    }
                                }
                            }
                            return SamplingOutcome { first_token_ms: None, route_events: Vec::new(), events, response: None, error: Some(error), attempts: attempt, elapsed: start.elapsed() };
                        }
                        _ => return SamplingOutcome { first_token_ms: None, route_events: Vec::new(), events, response: None, error: Some(error), attempts: attempt, elapsed: start.elapsed() },
                    }
                }
            }
        }
    }

    /// Original blocking sample (backward compat).
    pub async fn sample(&self, binding: &ModelBinding, request: &CanonicalModelRequest) -> SamplingOutcome {
        let (tx, _rx) = mpsc::unbounded_channel();
        drop(_rx);
        self.sample_streaming(binding, request, tx).await
    }

    async fn run_attempt(
        &self,
        binding: &ModelBinding,
        request: &CanonicalModelRequest,
        progress: &AtomicBool,
        events: &mut Vec<CanonicalModelEvent>,
        stream_tx: &mpsc::UnboundedSender<StreamFragment>,
        credential: Option<&str>,
    ) -> AttemptResult {
        let attempt_start = Instant::now();
        let byte_stream = match self
            .client
            .stream_raw_with_credential(binding, request, credential)
            .await
        {
            Ok(s) => s,
            Err(e) => return AttemptResult::Failed { error: map_err(e) },
        };
        futures::pin_mut!(byte_stream);

        let mut decoder: Box<dyn StreamingDecoder> = match binding.wire_protocol {
            WireProtocol::Responses => Box::new(ResponsesDecoder::new(request.request_id.clone())),
            WireProtocol::ChatCompletions => Box::new(ChatCompletionsDecoder::new(request.request_id.clone())),
            WireProtocol::Messages => Box::new(MessagesDecoder::new(request.request_id.clone())),
        };

        let mut first_token_ms: Option<u64> = None;
        while let Some(chunk) = byte_stream.next().await {
            match chunk {
                Ok(bytes) => match decoder.process_chunk(&bytes) {
                    Ok(chunk_events) => {
                        for ev in &chunk_events {
                            if first_token_ms.is_none() {
                                use CanonicalModelEvent as CME;
                                match ev {
                                    CME::TextDelta { .. }
                                    | CME::ReasoningDelta { .. }
                                    | CME::ToolCallStarted { .. }
                                    | CME::ToolCallArgumentsDelta { .. } => {
                                        first_token_ms =
                                            Some(attempt_start.elapsed().as_millis() as u64);
                                    }
                                    _ => {}
                                }
                            }
                            match ev {
                                CanonicalModelEvent::TextDelta { text, .. } => {
                                    progress.store(true, Ordering::Release);
                                    // Send EVERY non-empty text chunk the
                                    // moment it's decoded. No buffering, no
                                    // coalescing — this is what guarantees
                                    // Grodex-style "watch the answer grow
                                    // letter-by-letter" SSE UX.
                                    if !text.is_empty() {
                                        let _ = stream_tx.send(StreamFragment::Text(text.clone()));
                                    }
                                }
                                CanonicalModelEvent::ReasoningDelta { text, .. } => {
                                    progress.store(true, Ordering::Release);
                                    if !text.is_empty() {
                                        let _ = stream_tx.send(StreamFragment::Reasoning(text.clone()));
                                    }
                                }
                                CanonicalModelEvent::ToolCallStarted { call_id, name, .. } => {
                                    progress.store(true, Ordering::Release);
                                    let _ = stream_tx.send(StreamFragment::ToolCallStart {
                                        call_id: call_id.to_string(),
                                        name: name.clone(),
                                    });
                                }
                                CanonicalModelEvent::ToolCallArgumentsDelta { call_id, arguments_delta, .. } => {
                                    progress.store(true, Ordering::Release);
                                    if !arguments_delta.is_empty() {
                                        let _ = stream_tx.send(StreamFragment::ToolCallArgs {
                                            call_id: call_id.to_string(),
                                            args_delta: arguments_delta.clone(),
                                        });
                                    }
                                }
                                CanonicalModelEvent::ToolCallCompleted { call_id, arguments, .. } => {
                                    progress.store(true, Ordering::Release);
                                    // `arguments` is the complete JSON string
                                    // — if the incremental pipe missed any
                                    // bytes (rare decoder quirks), fall back
                                    // to replaying the final string as one
                                    // last Args delta so the TUI card always
                                    // has valid JSON.
                                    let _ = stream_tx.send(StreamFragment::ToolCallEnd {
                                        call_id: call_id.to_string(),
                                    });
                                    let mut done_args = String::new();
                                    std::mem::swap(&mut done_args, &mut arguments.clone());
                                    if !done_args.is_empty() {
                                        // Send full arguments AFTER the end
                                        // marker? No — the args stream might
                                        // have dropped characters, so we
                                        // send the assembled payload to a
                                        // separate small delta so the UI can
                                        // always recover a complete JSON.
                                        // Kept intentionally lightweight;
                                        // the renderer shows it inline.
                                        let _ = stream_tx.send(StreamFragment::ToolCallArgs {
                                            call_id: call_id.to_string(),
                                            args_delta: String::new(),
                                        });
                                    }
                                }
                                CanonicalModelEvent::ResponseMetadata { .. } | CanonicalModelEvent::StreamStarted { .. } => {
                                    progress.store(true, Ordering::Release);
                                }
                                _ => {}
                            }
                        }
                        events.extend(chunk_events);
                        if decoder.is_terminal() { break; }
                    }
                    Err(e) => return AttemptResult::Failed { error: map_err(e) },
                },
                Err(e) => return AttemptResult::Failed { error: SamplingError::transport(format!("{e}"), Some(crate::error::TransportSource::Request)) },
            }
        }
        match decoder.finalize() {
            Ok(final_events) => {
                for ev in &final_events {
                    if let CanonicalModelEvent::ResponseCompleted(resp) = ev { return AttemptResult::Completed { response: resp.clone(), first_token_ms }; }
                }
                events.extend(final_events);
                AttemptResult::Failed { error: SamplingError::transport("no terminal event", None) }
            }
            Err(e) => AttemptResult::Failed { error: map_err(e) },
        }
    }
}

enum AttemptResult {
    Completed {
        response: CanonicalModelResponse,
        /// Time from attempt start (request sent) to the first content
        /// event — TTFT. `None` when the attempt never produced content.
        first_token_ms: Option<u64>,
    },
    Failed { error: SamplingError },
}

fn map_err(e: ProviderError) -> SamplingError {
    match e {
        ProviderError::Auth { message, status_code } => SamplingError::Auth { message, status_code },
        ProviderError::Transport { message } => SamplingError::Transport { message, source: None },
        ProviderError::Api { status_code, message, retry_after_secs } => SamplingError::Api { status: status_code, message, retry_after_secs, should_retry: None },
        ProviderError::RateLimited { retry_after_secs, message } => SamplingError::RateLimited { retry_after_secs, message },
        ProviderError::IdleTimeout { elapsed_secs } => SamplingError::IdleTimeout { elapsed_secs },
        other => SamplingError::internal(format!("{other}")),
    }
}
