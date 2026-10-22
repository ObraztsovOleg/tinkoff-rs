-- Tokens and cost of every model request: decisions, previews and background
-- work (document tagging, embeddings). The daily budget sums a Moscow day.
CREATE TABLE ai_usage (
    id BIGSERIAL PRIMARY KEY,
    at TIMESTAMPTZ NOT NULL DEFAULT now(),
    purpose TEXT NOT NULL,
    strategy_id TEXT,
    profile TEXT,
    model TEXT,
    requests INTEGER NOT NULL DEFAULT 0,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cache_read_tokens BIGINT NOT NULL DEFAULT 0,
    cache_write_tokens BIGINT NOT NULL DEFAULT 0,
    web_searches INTEGER NOT NULL DEFAULT 0,
    cost_usd NUMERIC(14,6)
);
CREATE INDEX ai_usage_at_idx ON ai_usage(at);
ALTER TABLE ai_decisions ADD COLUMN usage JSONB, ADD COLUMN cost_usd NUMERIC(14,6);

-- A revision or a real-account copy continues the model memory of its parent:
-- active notes are copied once, past decisions are read up to the fork.
CREATE TABLE strategy_lineage (
    strategy_id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL,
    relation TEXT NOT NULL CHECK (relation IN ('revision','promotion')),
    forked_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE ai_memory ADD COLUMN inherited_from TEXT;
