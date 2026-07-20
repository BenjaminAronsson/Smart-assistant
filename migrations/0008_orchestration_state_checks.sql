-- Enforce the RunState / outcome vocabularies at the DB boundary, matching the
-- CHECK-constraint convention of the sibling tables (0002 sessions.status,
-- messages.role). Additive: ALTER ... ADD CONSTRAINT on the F1.4 tables — 0006
-- is already applied and is never edited (sqlx-data skill §1). Read-time
-- mapping in infra already rejects bad values; this catches a bad *write*
-- (future migration bug, manual psql) at insert time instead of next load.

ALTER TABLE orchestration.runs
    ADD CONSTRAINT runs_state_check CHECK (state IN (
        'received', 'context_ready', 'model_running', 'policy_review',
        'waiting_approval', 'tool_running', 'replanning', 'responding',
        'completed', 'failed', 'cancelled')),
    ADD CONSTRAINT runs_outcome_kind_check CHECK (
        outcome_kind IS NULL OR outcome_kind IN ('completed', 'failed', 'cancelled'));

ALTER TABLE orchestration.checkpoints
    ADD CONSTRAINT checkpoints_state_check CHECK (state IN (
        'received', 'context_ready', 'model_running', 'policy_review',
        'waiting_approval', 'tool_running', 'replanning', 'responding',
        'completed', 'failed', 'cancelled'));
