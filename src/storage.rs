//! Metrics persistence — save, load, and list session metrics from disk.

use std::path::Path;

use anyhow::{Context, Result};

use crate::types::{SessionMetrics, SessionSummary};

/// Metrics file name within a session directory.
const METRICS_FILE: &str = "session_metrics.json";

/// Incrementally write session metrics to `session_metrics.json`.
/// Overwrites the file on each call — safe for per-turn writes.
pub fn save_metrics(metrics: &SessionMetrics, session_dir: &Path) -> Result<()> {
    let path = session_dir.join(METRICS_FILE);
    let json = serde_json::to_string_pretty(metrics)?;
    std::fs::write(&path, json)?;
    tracing::debug!(path = %path.display(), turns = metrics.total_turns, "session_metrics saved");
    Ok(())
}

/// Load session metrics from `session_metrics.json` in a session directory.
pub fn load_metrics(session_dir: &Path) -> Result<SessionMetrics> {
    let path = session_dir.join(METRICS_FILE);
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read metrics file: {}", path.display()))?;
    let metrics: SessionMetrics = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse metrics file: {}", path.display()))?;
    Ok(metrics)
}

/// Try to load session metrics, returning `None` if the file doesn't exist.
pub fn try_load_metrics(session_dir: &Path) -> Option<SessionMetrics> {
    load_metrics(session_dir).ok()
}

/// List all session summaries by scanning the sessions directory.
/// Reads each `session_metrics.json` and extracts summary fields.
pub fn list_all_metrics(base_dir: &Path) -> Result<Vec<SessionSummary>> {
    let sessions_dir = base_dir.join("sessions");
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let mut summaries = Vec::new();
    for entry in std::fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        if let Some(metrics) = try_load_metrics(&path) {
            let product = metrics
                .custom
                .get("product")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            summaries.push(SessionSummary {
                session_id: metrics.session_id,
                node_id: metrics.node_id,
                created_at: metrics.created_at,
                model: metrics.model,
                total_turns: metrics.total_turns,
                total_chars: metrics.total_chars,
                outcome: metrics.outcome,
                product,
            });
        }
    }

    // Sort by created_at descending (newest first)
    summaries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SessionMetrics, SessionOutcome, TurnMetrics, TurnOutcome};
    use serde_json::Value;
    use std::fs;

    /// Helper: create a temporary directory for tests.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "phi_telemetry_test_{}_{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Helper: build a sample SessionMetrics with one turn.
    fn sample_metrics(session_id: &str, model: &str) -> SessionMetrics {
        let mut m = SessionMetrics::new(
            session_id.to_string(),
            "node-1".to_string(),
            model.to_string(),
        );
        let turn = TurnMetrics::new(
            1,
            "2026-08-01T12:00:00Z".to_string(),
            1000,
            model.to_string(),
            "test input".to_string(),
            TurnOutcome::Completed,
        );
        m.append_turn(turn);
        m
    }

    // ── save_metrics ──

    #[test]
    fn save_metrics_creates_file() {
        let dir = temp_dir("save_creates");
        let m = sample_metrics("s1", "gpt-4o");
        save_metrics(&m, &dir).unwrap();

        let path = dir.join(METRICS_FILE);
        assert!(path.exists());

        // Verify content is valid JSON
        let content = fs::read_to_string(&path).unwrap();
        let parsed: SessionMetrics = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.session_id, "s1");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_metrics_overwrites_existing() {
        let dir = temp_dir("save_overwrite");
        let m1 = sample_metrics("s1", "gpt-4o");
        save_metrics(&m1, &dir).unwrap();

        let mut m2 = sample_metrics("s2", "claude-sonnet");
        m2.total_turns = 5;
        save_metrics(&m2, &dir).unwrap();

        let loaded = load_metrics(&dir).unwrap();
        assert_eq!(loaded.session_id, "s2");
        assert_eq!(loaded.total_turns, 5);

        let _ = fs::remove_dir_all(&dir);
    }

    // ── load_metrics ──

    #[test]
    fn load_metrics_roundtrip() {
        let dir = temp_dir("load_roundtrip");
        let original = sample_metrics("s1", "gpt-4o");
        save_metrics(&original, &dir).unwrap();

        let loaded = load_metrics(&dir).unwrap();
        assert_eq!(loaded.session_id, "s1");
        assert_eq!(loaded.model, "gpt-4o");
        assert_eq!(loaded.total_turns, 1);
        assert_eq!(loaded.turns.len(), 1);
        assert_eq!(loaded.node_id, "node-1");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_metrics_file_not_found() {
        let dir = temp_dir("load_not_found");
        let result = load_metrics(&dir);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to read metrics file")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_metrics_invalid_json() {
        let dir = temp_dir("load_invalid");
        fs::write(dir.join(METRICS_FILE), "not valid json").unwrap();

        let result = load_metrics(&dir);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to parse metrics file")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // ── try_load_metrics ──

    #[test]
    fn try_load_metrics_returns_some_when_exists() {
        let dir = temp_dir("try_some");
        let m = sample_metrics("s1", "gpt-4o");
        save_metrics(&m, &dir).unwrap();

        let result = try_load_metrics(&dir);
        assert!(result.is_some());
        assert_eq!(result.unwrap().session_id, "s1");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_load_metrics_returns_none_when_missing() {
        let dir = temp_dir("try_none");
        let result = try_load_metrics(&dir);
        assert!(result.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_load_metrics_returns_none_on_invalid_json() {
        let dir = temp_dir("try_invalid");
        fs::write(dir.join(METRICS_FILE), "not json").unwrap();

        let result = try_load_metrics(&dir);
        assert!(result.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    // ── list_all_metrics ──

    #[test]
    fn list_all_metrics_empty_when_no_sessions_dir() {
        let dir = temp_dir("list_no_sessions");
        let result = list_all_metrics(&dir).unwrap();
        assert!(result.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_all_metrics_returns_summaries_sorted_by_date() {
        let base = temp_dir("list_sorted");
        let sessions_dir = base.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        // Create two sessions with different created_at
        let dir1 = sessions_dir.join("s1");
        fs::create_dir_all(&dir1).unwrap();
        let mut m1 = sample_metrics("s1", "gpt-4o");
        m1.created_at = "2026-01-01T00:00:00Z".to_string();
        save_metrics(&m1, &dir1).unwrap();

        let dir2 = sessions_dir.join("s2");
        fs::create_dir_all(&dir2).unwrap();
        let mut m2 = sample_metrics("s2", "claude-sonnet");
        m2.created_at = "2026-08-01T00:00:00Z".to_string();
        save_metrics(&m2, &dir2).unwrap();

        let summaries = list_all_metrics(&base).unwrap();
        assert_eq!(summaries.len(), 2);
        // Sorted descending by created_at — s2 first
        assert_eq!(summaries[0].session_id, "s2");
        assert_eq!(summaries[1].session_id, "s1");
        assert_eq!(summaries[0].model, "claude-sonnet");
        assert_eq!(summaries[0].total_turns, 1);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_all_metrics_skips_non_session_dirs() {
        let base = temp_dir("list_skip");
        let sessions_dir = base.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        // A subdirectory without metrics — should be skipped gracefully
        let empty_dir = sessions_dir.join("empty_session");
        fs::create_dir_all(&empty_dir).unwrap();

        // A regular file (not a dir) — should be skipped
        fs::write(sessions_dir.join("not_a_dir.txt"), "data").unwrap();

        // A valid session
        let valid_dir = sessions_dir.join("valid");
        fs::create_dir_all(&valid_dir).unwrap();
        save_metrics(&sample_metrics("valid", "gpt-4o"), &valid_dir).unwrap();

        let summaries = list_all_metrics(&base).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, "valid");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_all_metrics_extracts_product_from_custom() {
        let base = temp_dir("list_product");
        let sessions_dir = base.join("sessions");
        let sdir = sessions_dir.join("s1");
        fs::create_dir_all(&sdir).unwrap();

        let mut m = sample_metrics("s1", "gpt-4o");
        if let Value::Object(ref mut map) = m.custom {
            map.insert("product".to_string(), Value::String("phi-bard".to_string()));
        }
        save_metrics(&m, &sdir).unwrap();

        let summaries = list_all_metrics(&base).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].product, Some("phi-bard".to_string()));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_all_metrics_no_product_when_custom_empty() {
        let base = temp_dir("list_no_product");
        let sessions_dir = base.join("sessions");
        let sdir = sessions_dir.join("s1");
        fs::create_dir_all(&sdir).unwrap();

        let m = sample_metrics("s1", "gpt-4o");
        save_metrics(&m, &sdir).unwrap();

        let summaries = list_all_metrics(&base).unwrap();
        assert_eq!(summaries[0].product, None);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_all_metrics_summary_fields_are_correct() {
        let base = temp_dir("list_fields");
        let sessions_dir = base.join("sessions");
        let sdir = sessions_dir.join("s1");
        fs::create_dir_all(&sdir).unwrap();

        let mut m = sample_metrics("s1", "gpt-4o");
        m.node_id = "node-42".to_string();
        m.created_at = "2026-06-15T10:00:00Z".to_string();
        m.total_chars = 5000;
        m.outcome = SessionOutcome::Completed;
        save_metrics(&m, &sdir).unwrap();

        let summaries = list_all_metrics(&base).unwrap();
        let s = &summaries[0];
        assert_eq!(s.session_id, "s1");
        assert_eq!(s.node_id, "node-42");
        assert_eq!(s.created_at, "2026-06-15T10:00:00Z");
        assert_eq!(s.model, "gpt-4o");
        assert_eq!(s.total_turns, 1);
        assert_eq!(s.total_chars, 5000);
        assert_eq!(s.outcome, SessionOutcome::Completed);

        let _ = fs::remove_dir_all(&base);
    }
}
