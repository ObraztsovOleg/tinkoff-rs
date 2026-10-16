-- Preserve the immutable request identity while routing pending commands to a
-- fresh strategy revision after a flat account has changed owners.
ALTER TABLE control_commands ADD COLUMN execution_strategy_id TEXT;
ALTER TABLE strategy_controls ADD COLUMN superseded_by TEXT;
CREATE INDEX control_commands_execution_owner
    ON control_commands ((COALESCE(execution_strategy_id,strategy_id)),created_at)
    WHERE status IN ('queued','running');
