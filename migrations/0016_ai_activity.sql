-- What the AI model of each portfolio is doing now: thinking since when and why,
-- or idle with the outcome of the last request. One row per strategy.
CREATE TABLE ai_activity (
    strategy_id TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('thinking', 'idle')),
    trigger TEXT,
    details TEXT,
    profile TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    status TEXT
);
