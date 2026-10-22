-- Structured ideas of the AI model: instrument, direction, rationale, horizon and
-- exit conditions (price levels, a date), with the result when closed.
CREATE TABLE ai_theses (
    id UUID PRIMARY KEY,
    strategy_id TEXT NOT NULL,
    ticker TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('long','short','avoid')),
    thesis TEXT NOT NULL,
    horizon_days INTEGER,
    entry_price NUMERIC(20,6),
    conditions JSONB NOT NULL DEFAULT '[]',
    sources JSONB NOT NULL DEFAULT '[]',
    opened_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at TIMESTAMPTZ,
    close_reason TEXT,
    close_price NUMERIC(20,6),
    result_percent NUMERIC(10,3),
    inherited_from TEXT
);
CREATE INDEX ai_theses_open_idx ON ai_theses(strategy_id) WHERE closed_at IS NULL;
CREATE INDEX ai_theses_strategy_idx ON ai_theses(strategy_id, opened_at DESC);

-- How each decision turned out: return of its target weights and of the Moscow
-- Exchange index over 1, 5 and 20 trading days.
ALTER TABLE ai_decisions ADD COLUMN prices JSONB;
CREATE TABLE ai_outcomes (
    decision_id UUID NOT NULL REFERENCES ai_decisions(id) ON DELETE CASCADE,
    horizon_days INTEGER NOT NULL,
    portfolio_return NUMERIC(10,4),
    benchmark_return NUMERIC(10,4),
    computed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (decision_id, horizon_days)
);

-- Lessons of the weekly review and summaries of compacted notes live next to notes.
ALTER TABLE ai_memory ADD COLUMN kind TEXT NOT NULL DEFAULT 'note' CHECK (kind IN ('note','lesson','summary'));
CREATE TABLE ai_reviews (
    strategy_id TEXT PRIMARY KEY,
    reviewed_at TIMESTAMPTZ NOT NULL
);
