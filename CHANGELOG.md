# Changelog

All notable changes to phi-telemetry.

## [0.7.0] — 2026-09-27

### Added

- `SessionMetrics::merge_turns` + merge-on-save: a second process working over
  the same session (in-TUI `/resume`) extends the metrics instead of clobbering
  them — turns dedupe on `(turn_number, started_at)`, aggregates rebuild from
  the turn list, identity and custom fields are preserved, outcome follows the
  current writer.

### Changed

- Bump `agent-base` to 0.8.0.

## [0.6.0] — 2026-09-18

### Changed

- Bump `agent-base` to 0.7.0.

## [0.5.0] — 2026-09-11

### Changed

- Bump `agent-base` to 0.6.0.

## [0.4.0] — 2026-09-06

### Added

- `thinking_bytes` / `total_thinking_bytes` on turn metrics — reasoning-token
  byte accounting alongside the existing token counters.

## [0.3.0] — 2026-08-29

### Changed

- Bump `agent-base` to 0.4.0.

## [0.2.0] — 2026-08-14

### Changed

- Bump `agent-base` to 0.2.0 (Tool API v2). Minimal migration — telemetry types are unaffected; `TurnContext` / `AgentRuntime` / `RunOutcome` remain.

## [0.1.0] — 2026-07-30

Initial release.

### Added

- `TurnMetrics` / `SessionMetrics` / `SessionSummary` types with serde support
- `init_telemetry()` — channel-isolated observer via `on_turn_end` hook
- `ObserverHandle` with async `shutdown()` for graceful finalization
- `save_metrics()` / `load_metrics()` / `try_load_metrics()` / `list_all_metrics()` — JSON persistence
- `PHI_NODE_ID` / `PHI_METRICS_ENABLED` / `PHI_COST_PER_1K_TOKENS` env var support
- `phi metrics list` / `show` / `last` CLI commands
- Apache 2.0 license
