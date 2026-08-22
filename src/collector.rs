//! Metrics collector — subscribes to turn-end hooks and accumulates session metrics.
//!
//! Runs in an independent tokio task via channel isolation so that observer
//! panics never affect the agent runtime.

use agent_base::TurnContext;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing;

use crate::types::{SessionMetrics, TurnMetrics, run_outcome_to_turn_outcome};

/// Message sent from the hook callback to the observer task.
#[derive(Clone, Debug)]
pub(crate) enum ObserverMsg {
    /// A turn has completed — build TurnMetrics and accumulate.
    TurnEnd(TurnContext),
    /// Set session-level custom data.
    SetSessionCustom(Value),
    /// Shut down the observer and finalize metrics.
    Shutdown,
}

/// Handle to the background observer task.
///
/// Call [`shutdown`](Self::shutdown) to gracefully stop the observer and wait
/// for all pending metrics to be processed. Dropping without calling `shutdown`
/// detaches the task — metrics from the final turns may be lost.
pub struct ObserverHandle {
    tx: mpsc::UnboundedSender<ObserverMsg>,
    /// Shared session metrics, readable for external use (e.g. CLI display).
    pub session: Arc<RwLock<SessionMetrics>>,
    /// Handle to the observer task. None after `shutdown()` has been called.
    task: Option<JoinHandle<()>>,
    /// Pending turn-level custom data. Set by the consumer (e.g. phi-bard)
    /// via `on_turn_end`, consumed by the observer task when building TurnMetrics.
    pending_turn_custom: Arc<Mutex<Option<Value>>>,
}

impl ObserverHandle {
    /// Send session-level custom data to the observer.
    pub fn set_session_custom(&self, custom: Value) {
        let _ = self.tx.send(ObserverMsg::SetSessionCustom(custom));
    }

    /// Set turn-level custom data for the NEXT turn that the observer processes.
    ///
    /// Call this from an `on_turn_end` callback. The custom value will be
    /// merged into that turn's `custom` field in `TurnMetrics`.
    ///
    /// ```ignore
    /// agent.runtime().on_turn_end(move |_ctx| {
    ///     handle.set_turn_custom(json!({"check_quality_passed": true}));
    /// });
    /// ```
    pub fn set_turn_custom(&self, custom: Value) {
        if let Ok(mut pending) = self.pending_turn_custom.lock() {
            *pending = Some(custom);
        }
    }

    /// Shut down the observer gracefully and wait for all pending metrics
    /// to be processed. Returns once the observer task has exited.
    ///
    /// After this call, [`session`](Self::session) contains the final
    /// accumulated metrics (minus any `finalize()` call, which the caller
    /// should perform separately).
    pub async fn shutdown(&mut self) {
        let _ = self.tx.send(ObserverMsg::Shutdown);
        if let Some(task) = self.task.take() {
            // Don't propagate panic — observer panics must not crash the caller.
            let _ = task.await;
        }
    }
}

/// Initialise telemetry for an agent runtime.
///
/// Registers an `on_turn_end` hook that sends `TurnContext` data through a
/// channel to an independent observer task. The observer task builds
/// `TurnMetrics` and accumulates `SessionMetrics` — it never blocks the
/// agent runtime hot path.
///
/// Returns an [`ObserverHandle`] that can be used to inject custom data or
/// shut down the observer.
pub fn init_telemetry(
    runtime: &agent_base::AgentRuntime,
    session_id: String,
    node_id: String,
    model: String,
) -> ObserverHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<ObserverMsg>();

    // Hook: only sends a message — no heavy work on the hot path.
    let hook_tx = tx.clone();
    runtime.on_turn_end(move |ctx: &TurnContext| {
        let _ = hook_tx.send(ObserverMsg::TurnEnd(ctx.clone()));
    });

    let session = Arc::new(RwLock::new(SessionMetrics::new(session_id, node_id, model)));

    let observer_session = session.clone();
    let pending_turn_custom = Arc::new(Mutex::new(None::<Value>));
    let pending = pending_turn_custom.clone();

    // Independent task: build metrics from TurnContext
    let task = tokio::spawn(async move {
        let accumulator = observer_session;
        while let Some(msg) = rx.recv().await {
            match msg {
                ObserverMsg::TurnEnd(ctx) => {
                    let turn = {
                        let mut turn = build_turn_metrics(&ctx);
                        // Apply any pending turn-level custom data
                        if let Ok(mut pending) = pending.lock()
                            && let Some(custom) = pending.take()
                            && let Value::Object(ref mut map) = turn.custom
                            && let Value::Object(custom_map) = custom
                        {
                            for (k, v) in custom_map {
                                map.insert(k, v);
                            }
                        }
                        turn
                    };
                    let mut session = accumulator.write().await;
                    session.append_turn(turn);
                    tracing::debug!(turn = session.total_turns, "metrics: turn accumulated");
                }
                ObserverMsg::SetSessionCustom(custom) => {
                    let mut session = accumulator.write().await;
                    if let Value::Object(ref mut map) = session.custom
                        && let Value::Object(custom_map) = custom
                    {
                        for (k, v) in custom_map {
                            map.insert(k, v);
                        }
                    }
                }
                ObserverMsg::Shutdown => {
                    tracing::debug!("metrics: observer shutting down");
                    break;
                }
            }
        }
    });

    ObserverHandle {
        tx,
        session,
        task: Some(task),
        pending_turn_custom,
    }
}

/// Build a TurnMetrics from the raw TurnContext.
pub(crate) fn build_turn_metrics(ctx: &TurnContext) -> TurnMetrics {
    let input_tokens = ctx
        .usage
        .as_ref()
        .and_then(|u| u.prompt_tokens)
        .unwrap_or(0) as u64;
    let output_tokens = ctx
        .usage
        .as_ref()
        .and_then(|u| u.completion_tokens)
        .unwrap_or(0) as u64;

    let duration_ms = ctx.duration_ms;

    let turn_outcome = run_outcome_to_turn_outcome(&ctx.outcome, &ctx.tools_used);

    let mut turn = TurnMetrics::new(
        ctx.turn_number,
        chrono::Utc::now().to_rfc3339(),
        duration_ms,
        ctx.model.clone(),
        ctx.user_input.clone(),
        turn_outcome,
    );

    turn.time_to_first_token_ms = ctx.ttft_ms;
    turn.llm_duration_ms = ctx.llm_duration_ms;
    turn.tool_duration_ms = ctx.tool_duration_ms;
    turn.input_tokens = input_tokens;
    turn.output_tokens = output_tokens;
    turn.tool_call_count = ctx.tool_call_count;
    turn.tools_used = ctx.tools_used.clone();
    turn.tool_success = ctx.tool_success;
    turn.tool_failed = ctx.tool_failed;
    turn.text_length = ctx.full_text_len;
    turn.has_thinking = ctx.has_thinking;
    turn.plan_updates = ctx.plan_updates;
    turn.approval_count = ctx.approval_count;
    turn.llm_calls = ctx.llm_calls;
    if let Some(ref msg) = ctx.error_message {
        turn.error_message = Some(msg.clone());
    }

    turn
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TurnOutcome;
    use agent_base::llm::{LlmCapabilities, StreamChunk, UsageInfo};
    use agent_base::types::{ChatMessage, ResponseFormat};
    use agent_base::{AgentBuilder, RunOutcome, StreamClient};
    use async_trait::async_trait;
    use serde_json::json;
    use std::pin::Pin;
    use std::sync::Arc;

    // ── Stub StreamClient for creating AgentRuntime ──

    struct StubClient;

    #[async_trait]
    impl StreamClient for StubClient {
        async fn stream(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
            _reasoning: Option<&agent_base::llm::ReasoningConfig>,
            _response_format: Option<&ResponseFormat>,
        ) -> agent_base::AgentResult<
            Pin<Box<dyn futures_core::Stream<Item = agent_base::AgentResult<StreamChunk>> + Send>>,
        > {
            Ok(Box::pin(futures_util::stream::empty()))
        }

        fn capabilities(&self) -> LlmCapabilities {
            LlmCapabilities::default()
        }
    }

    /// Helper: create an AgentRuntime for testing.
    fn test_runtime() -> agent_base::AgentRuntime {
        AgentBuilder::new(Arc::new(StubClient)).build().unwrap()
    }

    /// Helper: build a TurnContext with sensible defaults.
    fn make_ctx(turn_number: u32, outcome: RunOutcome, tools_used: Vec<String>) -> TurnContext {
        TurnContext {
            session_id: 1,
            turn_number,
            ttft_ms: 150,
            llm_duration_ms: 800,
            duration_ms: 1200,
            tool_duration_ms: 200,
            usage: Some(UsageInfo {
                prompt_tokens: Some(500),
                completion_tokens: Some(300),
                total_tokens: Some(800),
            }),
            full_text_len: 1024,
            has_thinking: false,
            tools_used,
            tool_call_count: 1,
            tool_success: 1,
            tool_failed: 0,
            outcome,
            error_message: None,
            user_input: "test input".to_string(),
            model: "gpt-4o".to_string(),
            plan_updates: 0,
            approval_count: 0,
            llm_calls: 1,
        }
    }

    // ── build_turn_metrics ──

    #[test]
    fn build_turn_metrics_basic() {
        let ctx = make_ctx(1, RunOutcome::Completed, vec![]);
        let turn = build_turn_metrics(&ctx);

        assert_eq!(turn.turn_number, 1);
        assert_eq!(turn.duration_ms, 1200);
        assert_eq!(turn.time_to_first_token_ms, 150);
        assert_eq!(turn.llm_duration_ms, 800);
        assert_eq!(turn.tool_duration_ms, 200);
        assert_eq!(turn.input_tokens, 500);
        assert_eq!(turn.output_tokens, 300);
        assert_eq!(turn.model, "gpt-4o");
        assert_eq!(turn.tool_call_count, 1);
        assert_eq!(turn.tool_success, 1);
        assert_eq!(turn.tool_failed, 0);
        assert_eq!(turn.text_length, 1024);
        assert_eq!(turn.outcome, TurnOutcome::Completed);
        assert_eq!(turn.user_input, "test input");
        assert_eq!(turn.llm_calls, 1);
        assert!(turn.error_message.is_none());
        assert!(!turn.has_thinking);
        assert_eq!(turn.plan_updates, 0);
        assert_eq!(turn.approval_count, 0);
    }

    #[test]
    fn build_turn_metrics_with_tools() {
        let ctx = make_ctx(
            2,
            RunOutcome::Completed,
            vec!["shell".to_string(), "read_file".to_string()],
        );
        let turn = build_turn_metrics(&ctx);

        assert_eq!(turn.outcome, TurnOutcome::ToolCalls);
        assert_eq!(turn.tools_used, vec!["shell", "read_file"]);
    }

    #[test]
    fn build_turn_metrics_error_outcome() {
        let ctx = make_ctx(
            1,
            RunOutcome::Failed {
                error: "oops".to_string(),
            },
            vec![],
        );
        let turn = build_turn_metrics(&ctx);
        assert_eq!(turn.outcome, TurnOutcome::Error);
    }

    #[test]
    fn build_turn_metrics_cancelled() {
        let ctx = make_ctx(1, RunOutcome::Cancelled, vec![]);
        let turn = build_turn_metrics(&ctx);
        assert_eq!(turn.outcome, TurnOutcome::Cancelled);
    }

    #[test]
    fn build_turn_metrics_max_turns() {
        let ctx = make_ctx(1, RunOutcome::MaxTurnsExceeded { turns: 10 }, vec![]);
        let turn = build_turn_metrics(&ctx);
        assert_eq!(turn.outcome, TurnOutcome::MaxTurns);
    }

    #[test]
    fn build_turn_metrics_continuing() {
        let ctx = make_ctx(1, RunOutcome::Continuing, vec![]);
        let turn = build_turn_metrics(&ctx);
        assert_eq!(turn.outcome, TurnOutcome::Completed);
    }

    #[test]
    fn build_turn_metrics_no_usage() {
        let mut ctx = make_ctx(1, RunOutcome::Completed, vec![]);
        ctx.usage = None;
        let turn = build_turn_metrics(&ctx);
        assert_eq!(turn.input_tokens, 0);
        assert_eq!(turn.output_tokens, 0);
    }

    #[test]
    fn build_turn_metrics_partial_usage() {
        let mut ctx = make_ctx(1, RunOutcome::Completed, vec![]);
        ctx.usage = Some(UsageInfo {
            prompt_tokens: Some(100),
            completion_tokens: None,
            total_tokens: None,
        });
        let turn = build_turn_metrics(&ctx);
        assert_eq!(turn.input_tokens, 100);
        assert_eq!(turn.output_tokens, 0);
    }

    #[test]
    fn build_turn_metrics_with_error_message() {
        let mut ctx = make_ctx(1, RunOutcome::Completed, vec![]);
        ctx.error_message = Some("connection timeout".to_string());
        let turn = build_turn_metrics(&ctx);
        assert_eq!(turn.error_message, Some("connection timeout".to_string()));
    }

    #[test]
    fn build_turn_metrics_with_thinking() {
        let mut ctx = make_ctx(1, RunOutcome::Completed, vec![]);
        ctx.has_thinking = true;
        let turn = build_turn_metrics(&ctx);
        assert!(turn.has_thinking);
    }

    #[test]
    fn build_turn_metrics_with_meta_events() {
        let mut ctx = make_ctx(1, RunOutcome::Completed, vec![]);
        ctx.plan_updates = 3;
        ctx.approval_count = 2;
        let turn = build_turn_metrics(&ctx);
        assert_eq!(turn.plan_updates, 3);
        assert_eq!(turn.approval_count, 2);
    }

    #[test]
    fn build_turn_metrics_user_input_truncated() {
        let mut ctx = make_ctx(1, RunOutcome::Completed, vec![]);
        ctx.user_input = "a".repeat(200);
        let turn = build_turn_metrics(&ctx);
        // TurnMetrics::new truncates to 80 chars + "..."
        assert!(turn.user_input.len() <= 83);
        assert!(turn.user_input.ends_with("..."));
    }

    // ── ObserverHandle ──

    #[tokio::test]
    async fn observer_handle_set_session_custom() {
        let runtime = test_runtime();
        let mut handle = init_telemetry(
            &runtime,
            "test".to_string(),
            "".to_string(),
            "gpt-4o".to_string(),
        );

        handle.set_session_custom(json!({"product": "phi-bard"}));

        // Give observer time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        {
            let session = handle.session.read().await;
            assert_eq!(
                session.custom.get("product").and_then(|v| v.as_str()),
                Some("phi-bard")
            );
        }

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn observer_handle_shutdown() {
        let runtime = test_runtime();
        let mut handle = init_telemetry(
            &runtime,
            "test".to_string(),
            "".to_string(),
            "gpt-4o".to_string(),
        );

        // Shutdown should complete without hanging
        handle.shutdown().await;

        // After shutdown, session should still be accessible
        let session = handle.session.read().await;
        assert_eq!(session.session_id, "test");
        assert_eq!(session.model, "gpt-4o");
    }

    #[tokio::test]
    async fn observer_shutdown_idempotent() {
        let runtime = test_runtime();
        let mut handle = init_telemetry(
            &runtime,
            "test".to_string(),
            "".to_string(),
            "gpt-4o".to_string(),
        );

        handle.shutdown().await;
        // Second shutdown should not panic
        handle.shutdown().await;
    }

    // ── init_telemetry ──

    #[tokio::test]
    async fn init_telemetry_returns_handle() {
        let runtime = test_runtime();
        let handle = init_telemetry(
            &runtime,
            "s1".to_string(),
            "n1".to_string(),
            "gpt-4o".to_string(),
        );

        // Session should be initialized with provided values
        let session = handle.session.read().await;
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.node_id, "n1");
        assert_eq!(session.model, "gpt-4o");
        assert_eq!(session.total_turns, 0);
        assert!(session.turns.is_empty());
    }

    #[tokio::test]
    async fn observer_processes_turn_end_via_hook() {
        let runtime = test_runtime();
        let mut handle = init_telemetry(
            &runtime,
            "test".to_string(),
            "".to_string(),
            "gpt-4o".to_string(),
        );

        // Use set_session_custom to verify the observer task is running
        handle.set_session_custom(json!({"test": true}));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        {
            let session = handle.session.read().await;
            assert_eq!(session.custom.get("test"), Some(&json!(true)));
        }

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn observer_set_turn_custom_merges_into_next_turn() {
        let runtime = test_runtime();
        let mut handle = init_telemetry(
            &runtime,
            "test".to_string(),
            "".to_string(),
            "gpt-4o".to_string(),
        );

        // Set turn custom data — it should be stored in pending_turn_custom
        handle.set_turn_custom(json!({"quality": 0.95}));

        // Verify it's stored
        {
            let pending = handle.pending_turn_custom.lock().unwrap();
            assert!(pending.is_some());
            assert_eq!(
                pending
                    .as_ref()
                    .unwrap()
                    .get("quality")
                    .and_then(|v| v.as_f64()),
                Some(0.95)
            );
        }

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn observer_turn_custom_overwrites_previous() {
        let runtime = test_runtime();
        let mut handle = init_telemetry(
            &runtime,
            "test".to_string(),
            "".to_string(),
            "gpt-4o".to_string(),
        );

        handle.set_turn_custom(json!({"first": true}));
        handle.set_turn_custom(json!({"second": true}));

        {
            let pending = handle.pending_turn_custom.lock().unwrap();
            assert_eq!(
                pending
                    .as_ref()
                    .unwrap()
                    .get("second")
                    .and_then(|v| v.as_bool()),
                Some(true)
            );
            // First value was overwritten
            assert!(pending.as_ref().unwrap().get("first").is_none());
        }

        handle.shutdown().await;
    }
}
