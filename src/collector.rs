//! Metrics collector — subscribes to turn-end hooks and accumulates session metrics.
//!
//! Runs in an independent tokio task via channel isolation so that observer
//! panics never affect the agent runtime.

use agent_base::TurnContext;
use serde_json::Value;
use std::sync::Arc;
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
}

impl ObserverHandle {
    /// Send session-level custom data to the observer.
    pub fn set_session_custom(&self, custom: Value) {
        let _ = self.tx.send(ObserverMsg::SetSessionCustom(custom));
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

    // Independent task: build metrics from TurnContext
    let task = tokio::spawn(async move {
        let accumulator = observer_session;
        while let Some(msg) = rx.recv().await {
            match msg {
                ObserverMsg::TurnEnd(ctx) => {
                    let turn = build_turn_metrics(&ctx);
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
    }
}

/// Build a TurnMetrics from the raw TurnContext.
fn build_turn_metrics(ctx: &TurnContext) -> TurnMetrics {
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
