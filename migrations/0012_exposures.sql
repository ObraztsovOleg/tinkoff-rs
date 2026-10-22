-- How each instrument reacts to market topics: oil, sanctions, key rate, ruble…
-- A model drafts the profile once; the operator may edit it (origin 'operator'),
-- and the model never overwrites operator values.
CREATE TABLE instrument_profiles (
    ticker TEXT PRIMARY KEY,
    uid TEXT,
    name TEXT NOT NULL DEFAULT '',
    sector TEXT NOT NULL DEFAULT '',
    note TEXT NOT NULL DEFAULT '',
    drafted_at TIMESTAMPTZ,
    draft_error TEXT,
    confirmed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE instrument_exposures (
    ticker TEXT NOT NULL REFERENCES instrument_profiles(ticker) ON DELETE CASCADE,
    topic TEXT NOT NULL,
    sensitivity NUMERIC(3,1) NOT NULL CHECK (sensitivity BETWEEN -2 AND 2),
    note TEXT NOT NULL DEFAULT '',
    origin TEXT NOT NULL DEFAULT 'model' CHECK (origin IN ('model','operator')),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (ticker, topic)
);
-- Tagged documents by topic for search filters and macro triggers.
CREATE INDEX documents_topics_idx ON documents USING gin(topics jsonb_path_ops);
CREATE INDEX documents_tickers_idx ON documents USING gin(tickers);
