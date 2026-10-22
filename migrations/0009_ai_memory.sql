-- News collected by ai-model runtimes: RSS feeds keep only fresh items, this
-- table lets the model search days and weeks back.
CREATE TABLE news_items (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    title TEXT NOT NULL,
    link TEXT NOT NULL DEFAULT '',
    summary TEXT NOT NULL DEFAULT '',
    published_at TIMESTAMPTZ NOT NULL,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX news_items_published_idx ON news_items(published_at DESC);

-- Long-term notes the model keeps between decisions: theses, plans, warnings.
CREATE TABLE ai_memory (
    id UUID PRIMARY KEY,
    strategy_id TEXT NOT NULL,
    note TEXT NOT NULL,
    tickers JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ,
    closed_at TIMESTAMPTZ,
    closed_reason TEXT
);
CREATE INDEX ai_memory_active_idx ON ai_memory(strategy_id, created_at DESC) WHERE closed_at IS NULL;

-- Why the model was asked: session open, schedule, news burst, price jump, preview.
ALTER TABLE ai_decisions ADD COLUMN trigger TEXT;
ALTER TABLE ai_decisions ADD COLUMN hold BOOLEAN NOT NULL DEFAULT false;
