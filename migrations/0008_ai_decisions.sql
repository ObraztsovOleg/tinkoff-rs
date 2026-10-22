-- Every decision, preview and failure of an `ai-model` strategy: the model, its
-- weights, rationale, the news it read and the tools it called.
CREATE TABLE ai_decisions (
    id UUID PRIMARY KEY,
    strategy_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('decision','preview')),
    status TEXT NOT NULL CHECK (status IN ('ok','failed')),
    session_date DATE,
    profile TEXT,
    provider TEXT,
    model TEXT,
    weights JSONB NOT NULL DEFAULT '[]',
    rationale TEXT,
    sources JSONB NOT NULL DEFAULT '[]',
    tool_calls JSONB NOT NULL DEFAULT '{}',
    error TEXT,
    duration_ms INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ai_decisions_strategy_idx ON ai_decisions(strategy_id, created_at DESC);
