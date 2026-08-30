//! SQLite schema + migrations (`user_version` based, same pattern as
//! `grodex-memory`). The telemetry DB is a **query projection** of the
//! rollout journal, never a recovery source of truth.

use rusqlite::Connection;

pub const SCHEMA_VERSION: u32 = 4;

pub fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    let v: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if v >= SCHEMA_VERSION {
        return Ok(());
    }
    if v < 1 {
        conn.execute_batch(V1_DDL)?;
    }
    if v < 2 {
        conn.execute_batch(V2_DDL)?;
    }
    if v < 3 {
        conn.execute_batch(V3_DDL)?;
    }
    if v < 4 {
        conn.execute_batch(V4_DDL)?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

const V1_DDL: &str = r#"
        BEGIN;

        -- Raw event table: keeps every emitted record (including kinds the
        -- projection tables don't understand yet) so future migrations can
        -- rebuild projections from it. `journal_seq` aligns 1:1 with
        -- rollout.jsonl for crash-gap backfill.
        CREATE TABLE IF NOT EXISTS telemetry_events (
            event_id     TEXT PRIMARY KEY,
            run_id       TEXT NOT NULL,
            session_id   TEXT NOT NULL,
            turn_id      TEXT,
            step_id      TEXT,
            call_id      TEXT,
            journal_seq  INTEGER,
            kind         TEXT NOT NULL,
            status       TEXT,
            severity     TEXT NOT NULL DEFAULT 'info',
            occurred_at  TEXT NOT NULL,
            duration_ms  INTEGER,
            payload_json TEXT NOT NULL,
            sensitivity  TEXT NOT NULL DEFAULT 'normal'
        );
        CREATE INDEX IF NOT EXISTS idx_telem_session_time
            ON telemetry_events(session_id, occurred_at);
        CREATE INDEX IF NOT EXISTS idx_telem_turn
            ON telemetry_events(turn_id, occurred_at);
        CREATE INDEX IF NOT EXISTS idx_telem_kind_time
            ON telemetry_events(kind, occurred_at);

        -- One row per session (projection of SessionStarted).
        CREATE TABLE IF NOT EXISTS sessions (
            session_id     TEXT PRIMARY KEY,
            run_id         TEXT NOT NULL,
            started_at     TEXT,
            finished_at    TEXT,
            final_state    TEXT,
            cwd_hash       TEXT,
            model_provider TEXT,
            model          TEXT,
            recovery_count INTEGER NOT NULL DEFAULT 0
        );

        -- One row per Turn (projection of TurnStarted + TurnCompleted).
        -- `termination_reason` is structured: final_answer | repair_exhausted
        -- | step_budget_exhausted | cancelled | sampling_error | tool_error
        -- | journal_failure | indeterminate_wait.
        CREATE TABLE IF NOT EXISTS turns (
            turn_id            TEXT PRIMARY KEY,
            session_id         TEXT NOT NULL,
            run_id             TEXT NOT NULL DEFAULT '',
            started_at         TEXT,
            finished_at        TEXT,
            status             TEXT NOT NULL DEFAULT 'running',
            termination_reason TEXT,
            input_chars        INTEGER,
            steps              INTEGER,
            model_calls        INTEGER,
            tool_calls         INTEGER,
            retries            INTEGER,
            compactions        INTEGER,
            cancel_count       INTEGER,
            duration_ms        INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_turns_session
            ON turns(session_id, started_at);

        -- High-water mark of projected journal seqs, per session. Startup
        -- re-projection reads the journal from here (plus idempotent
        -- INSERT OR IGNORE, so a full replay from 0 is also safe).
        CREATE TABLE IF NOT EXISTS projection_cursors (
            source           TEXT NOT NULL,
            session_id       TEXT NOT NULL,
            last_journal_seq INTEGER NOT NULL,
            updated_at       TEXT NOT NULL,
            PRIMARY KEY (source, session_id)
        );

        COMMIT;
        "#;

/// P1 diagnostic projections: model attempts, tool lifecycle, security
/// decisions. All are rebuildable from the journal + `telemetry_events`.
const V2_DDL: &str = r#"
        BEGIN;

        -- One row per model sampling round. Answers: 哪个模型/供应商慢、
        -- 重试发生在哪、缓存命中率多少、错误集中在哪一类。
        CREATE TABLE IF NOT EXISTS model_attempts (
            attempt_id          TEXT PRIMARY KEY,  -- event_id of the Started event
            session_id          TEXT NOT NULL,
            run_id              TEXT NOT NULL DEFAULT '',
            turn_id             TEXT,
            step_id             TEXT,
            request_id          TEXT,
            provider            TEXT,
            model               TEXT,
            wire_protocol       TEXT,
            attempts            INTEGER,
            started_at          TEXT,
            finished_at         TEXT,
            duration_ms         INTEGER,
            status              TEXT,              -- running | ok | error
            error_class         TEXT,
            http_status         INTEGER,
            retry_after_secs    INTEGER,
            provider_request_id TEXT,
            input_tokens        INTEGER,
            cached_input_tokens INTEGER,
            cache_creation_tokens INTEGER,
            output_tokens       INTEGER,
            reasoning_tokens    INTEGER,
            total_tokens        INTEGER,
            estimated           INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_ma_session ON model_attempts(session_id, started_at);
        CREATE INDEX IF NOT EXISTS idx_ma_model   ON model_attempts(provider, model);

        -- One row per tool call lifecycle, assembled from the durable
        -- prepared → approved → started → finished → committed events.
        -- Answers: 审批等了多久、工具跑了多久、崩溃后停在哪个阶段、
        -- 哪个工具平均最慢。
        CREATE TABLE IF NOT EXISTS tool_executions (
            session_id       TEXT NOT NULL,
            call_id          TEXT NOT NULL,
            run_id           TEXT NOT NULL DEFAULT '',
            turn_id          TEXT,
            step_id          TEXT,
            tool_name        TEXT,
            operation_id     TEXT,
            prepared_at      TEXT,
            approved_at      TEXT,
            started_at       TEXT,
            finished_at      TEXT,
            committed_at     TEXT,
            approval_wait_ms INTEGER,
            duration_ms      INTEGER,
            exit_code        INTEGER,
            is_error         INTEGER,
            output_truncated INTEGER,
            status           TEXT NOT NULL DEFAULT 'prepared',
            PRIMARY KEY (session_id, call_id)
        );
        CREATE INDEX IF NOT EXISTS idx_te_session ON tool_executions(session_id, prepared_at);
        CREATE INDEX IF NOT EXISTS idx_te_name    ON tool_executions(tool_name);

        -- Permission / approval / lease / capability-stale decisions.
        -- Answers: 为什么这个工具没执行、为什么需要审批、为什么被沙箱拒绝。
        CREATE TABLE IF NOT EXISTS security_decisions (
            decision_id   TEXT PRIMARY KEY,  -- journal event_id
            session_id    TEXT NOT NULL,
            run_id        TEXT NOT NULL DEFAULT '',
            turn_id       TEXT,
            step_id       TEXT,
            call_id       TEXT,
            operation_id  TEXT,
            tool_name     TEXT,
            ticket_id     TEXT,
            lease_id      TEXT,
            decision_type TEXT NOT NULL,     -- approval_requested | approval_resolved
                                             -- | lease_issued | lease_consumed | lease_expired
                                             -- | capability_stale
            decision      TEXT,              -- approved | rejected | expired | narrowed
            reason        TEXT,
            occurred_at   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_sd_session ON security_decisions(session_id, occurred_at);
        CREATE INDEX IF NOT EXISTS idx_sd_call    ON security_decisions(call_id);

        COMMIT;
        "#;

/// P2: context/cost projections + query views. `first_token_ms` is added
/// to model_attempts via ALTER (the v2 table predates it).
const V3_DDL: &str = r#"
        BEGIN;

        ALTER TABLE model_attempts ADD COLUMN first_token_ms INTEGER;

        -- One row per sampling step's prompt shape: hashes + token
        -- estimates only, NEVER prompt content (kept out of telemetry.db
        -- by design; full content lives in the journal / blob store).
        CREATE TABLE IF NOT EXISTS prompt_builds (
            prompt_id             TEXT PRIMARY KEY,   -- journal event_id
            session_id            TEXT NOT NULL,
            run_id                TEXT NOT NULL DEFAULT '',
            turn_id               TEXT,
            step_id               TEXT,
            prompt_snapshot_hash  TEXT,
            context_item_count    INTEGER,
            estimated_input_tokens INTEGER,
            built_at              TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_pb_session ON prompt_builds(session_id, built_at);
        CREATE INDEX IF NOT EXISTS idx_pb_hash    ON prompt_builds(prompt_snapshot_hash);

        -- Compaction lifecycle: Started → (CandidateBuilt) → Committed/Failed.
        CREATE TABLE IF NOT EXISTS compactions (
            compaction_id  TEXT PRIMARY KEY,   -- event_id of the Started event
            session_id     TEXT NOT NULL,
            run_id         TEXT NOT NULL DEFAULT '',
            turn_id        TEXT,
            trigger        TEXT,
            started_at     TEXT,
            finished_at    TEXT,
            committed_at   TEXT,
            pre_item_count INTEGER,
            candidate_item_count INTEGER,
            status         TEXT NOT NULL DEFAULT 'started',
            failure_reason TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_comp_session ON compactions(session_id, started_at);

        COMMIT;
        "#;

/// Query views (created after every migration; CREATE OR REPLACE keeps
/// them in sync with the code's expected columns).
const VIEWS_DDL: &str = r#"
        BEGIN;

        CREATE VIEW IF NOT EXISTS v_session_timeline AS
        SELECT s.session_id, s.run_id, s.started_at AS session_started_at,
               s.model_provider, s.model,
               t.turn_id, t.started_at, t.finished_at, t.status,
               t.termination_reason, t.duration_ms,
               t.steps, t.model_calls, t.tool_calls, t.retries
        FROM sessions s LEFT JOIN turns t ON t.session_id = s.session_id
        ORDER BY t.started_at;

        CREATE VIEW IF NOT EXISTS v_turn_summary AS
        SELECT t.session_id, t.turn_id, t.status, t.termination_reason,
               t.steps, t.model_calls, t.tool_calls, t.retries, t.compactions,
               t.duration_ms, t.input_chars,
               (SELECT COUNT(*) FROM tool_executions te
                 WHERE te.session_id = t.session_id AND te.turn_id = t.turn_id
                   AND te.status NOT IN ('committed')) AS incomplete_tools,
               (SELECT COUNT(*) FROM model_attempts ma
                 WHERE ma.turn_id = t.turn_id AND ma.status = 'error') AS failed_attempts
        FROM turns t;

        CREATE VIEW IF NOT EXISTS v_tool_lifecycle AS
        SELECT session_id, call_id, tool_name, turn_id,
               prepared_at, approved_at, started_at, finished_at, committed_at,
               approval_wait_ms, duration_ms, exit_code, is_error, status,
               CASE
                 WHEN status = 'indeterminate' THEN 'indeterminate'
                 WHEN started_at IS NOT NULL AND finished_at IS NULL THEN 'stuck_running'
                 WHEN finished_at IS NOT NULL AND committed_at IS NULL THEN 'uncommitted'
                 ELSE 'ok'
               END AS anomaly
        FROM tool_executions;

        CREATE VIEW IF NOT EXISTS v_model_usage AS
        SELECT provider, model,
               COUNT(*) AS calls,
               SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) AS errors,
               AVG(duration_ms) AS avg_ms,
               MAX(duration_ms) AS max_ms,
               AVG(first_token_ms) AS avg_first_token_ms,
               SUM(COALESCE(input_tokens, 0)) AS input_tokens,
               SUM(COALESCE(cached_input_tokens, 0)) AS cached_input_tokens,
               SUM(COALESCE(cache_creation_tokens, 0)) AS cache_creation_tokens,
               SUM(COALESCE(output_tokens, 0)) AS output_tokens,
               SUM(COALESCE(reasoning_tokens, 0)) AS reasoning_tokens,
               SUM(COALESCE(total_tokens, 0)) AS total_tokens
        FROM model_attempts
        GROUP BY provider, model;

        CREATE VIEW IF NOT EXISTS v_cache_stats AS
        SELECT provider, model,
               SUM(COALESCE(cached_input_tokens, 0)) * 1.0
                 / NULLIF(SUM(COALESCE(input_tokens, 0)), 0) AS cache_hit_rate,
               SUM(COALESCE(cached_input_tokens, 0)) AS cached_input_tokens,
               SUM(COALESCE(cache_creation_tokens, 0)) AS cache_creation_tokens,
               SUM(COALESCE(input_tokens, 0)) AS input_tokens,
               COUNT(*) AS calls
        FROM model_attempts
        GROUP BY provider, model;

        CREATE VIEW IF NOT EXISTS v_recovery_anomalies AS
        SELECT 'open_turn' AS anomaly, session_id, turn_id AS subject_id, started_at AS occurred_at, NULL AS detail
          FROM turns WHERE finished_at IS NULL OR status = 'running'
        UNION ALL
        SELECT 'stuck_tool', session_id, call_id, started_at, tool_name
          FROM tool_executions WHERE started_at IS NOT NULL AND finished_at IS NULL
        UNION ALL
        SELECT 'uncommitted_result', session_id, call_id, finished_at, tool_name
          FROM tool_executions WHERE finished_at IS NOT NULL AND committed_at IS NULL
            AND status != 'indeterminate'
        UNION ALL
        SELECT 'indeterminate_tool', session_id, call_id, prepared_at, tool_name
          FROM tool_executions WHERE status = 'indeterminate';

        COMMIT;
        "#;

pub fn create_views(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(VIEWS_DDL)
}

/// P3: peripheral-module projections — sub-agents, skills, memory
/// retrievals, MCP lifecycle.
const V4_DDL: &str = r#"
        BEGIN;

        CREATE TABLE IF NOT EXISTS subagent_runs (
            session_id  TEXT NOT NULL,
            task_id     TEXT NOT NULL,
            run_id      TEXT NOT NULL DEFAULT '',
            agent_id    TEXT,
            parent_id   TEXT,
            label       TEXT,
            started_at  TEXT,
            finished_at TEXT,
            status      TEXT NOT NULL DEFAULT 'running',
            tokens      INTEGER,
            error       TEXT,
            PRIMARY KEY (session_id, task_id)
        );
        CREATE INDEX IF NOT EXISTS idx_sub_session ON subagent_runs(session_id, started_at);

        CREATE TABLE IF NOT EXISTS skill_activations (
            activation_id    TEXT PRIMARY KEY,  -- journal event_id + skill name
            session_id       TEXT NOT NULL,
            run_id           TEXT NOT NULL DEFAULT '',
            turn_id          TEXT,
            skill_name       TEXT NOT NULL,
            source           TEXT,
            path             TEXT,
            content_hash     TEXT,
            skill_generation INTEGER,
            loaded_at        TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_skill_session ON skill_activations(session_id, loaded_at);

        -- Out-of-band records (not journaled — measured at the call site).
        CREATE TABLE IF NOT EXISTS memory_retrievals (
            retrieval_id   TEXT PRIMARY KEY,
            session_id     TEXT NOT NULL,
            run_id         TEXT NOT NULL DEFAULT '',
            turn_id        TEXT,
            query_chars    INTEGER,
            selected_count INTEGER,
            duration_ms    INTEGER,
            router_kind    TEXT,
            occurred_at    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_memret_session ON memory_retrievals(session_id, occurred_at);

        CREATE TABLE IF NOT EXISTS mcp_lifecycle (
            event_id    TEXT PRIMARY KEY,
            session_id  TEXT NOT NULL,
            run_id      TEXT NOT NULL DEFAULT '',
            server_name TEXT NOT NULL,
            phase       TEXT NOT NULL,   -- spawn | list_tools | oauth_register
            transport   TEXT,
            tool_count  INTEGER,
            status      TEXT,            -- ok | failed
            error_class TEXT,
            duration_ms INTEGER,
            occurred_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_mcp_session ON mcp_lifecycle(session_id, occurred_at);

        COMMIT;
        "#;
