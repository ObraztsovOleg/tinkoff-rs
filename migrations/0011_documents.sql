-- Documents for the AI model: wire news with full texts, issuer disclosures,
-- articles, exchange and regulator notices, press releases and reports.
-- One event published by several agencies shares a cluster.
CREATE TABLE documents (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('news','article','disclosure','press_release','report','exchange','regulator')),
    url TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL,
    lead TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    tables JSONB NOT NULL DEFAULT '[]',
    published_at TIMESTAMPTZ NOT NULL,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Instruments named by the source (T-Invest tags) or found by the tagger.
    instruments TEXT[] NOT NULL DEFAULT '{}',
    tickers TEXT[] NOT NULL DEFAULT '{}',
    priority BOOLEAN NOT NULL DEFAULT false,
    content_hash TEXT NOT NULL,
    simhash BIGINT NOT NULL,
    cluster_id TEXT NOT NULL,
    -- Mark-up by a cheap model: relevance, event, topics, importance, summary.
    status TEXT NOT NULL DEFAULT 'new' CHECK (status IN ('new','tagged','skipped','failed')),
    importance SMALLINT,
    event_type TEXT,
    summary TEXT,
    topics JSONB NOT NULL DEFAULT '[]',
    facts JSONB,
    tagged_at TIMESTAMPTZ,
    -- The text of an old irrelevant document is dropped; the record stays for dedupe.
    body_dropped_at TIMESTAMPTZ
);
CREATE INDEX documents_published_idx ON documents(published_at DESC);
CREATE INDEX documents_cluster_idx ON documents(cluster_id);
CREATE INDEX documents_instruments_idx ON documents USING gin(instruments);
CREATE INDEX documents_hash_idx ON documents(content_hash);
CREATE INDEX documents_untagged_idx ON documents(published_at DESC) WHERE status = 'new';

-- Searchable pieces of ~1500 characters. `context` names the document (title,
-- source, date) so a piece found alone is still understandable.
CREATE TABLE document_chunks (
    document_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    ord INTEGER NOT NULL,
    text TEXT NOT NULL,
    context TEXT NOT NULL DEFAULT '',
    published_at TIMESTAMPTZ NOT NULL,
    search TSVECTOR GENERATED ALWAYS AS (
        setweight(to_tsvector('russian', context), 'B') || setweight(to_tsvector('russian', text), 'A')) STORED,
    PRIMARY KEY (document_id, ord)
);
CREATE INDEX document_chunks_search_idx ON document_chunks USING gin(search);
CREATE INDEX document_chunks_published_idx ON document_chunks(published_at DESC);

-- Progress of each source of the collector: cursor, last run and error.
CREATE TABLE collector_state (
    source TEXT PRIMARY KEY,
    cursor TEXT,
    last_run TIMESTAMPTZ,
    last_success TIMESTAMPTZ,
    last_error TEXT,
    added_total BIGINT NOT NULL DEFAULT 0,
    added_last INTEGER NOT NULL DEFAULT 0
);

-- Headlines collected by earlier versions become documents.
INSERT INTO documents(id,source,kind,url,title,lead,published_at,fetched_at,content_hash,simhash,cluster_id)
SELECT 'rss:' || id, source, 'news', link, title, summary, published_at, fetched_at, md5(lower(title)), 0, 'rss:' || id
FROM news_items ON CONFLICT DO NOTHING;
INSERT INTO document_chunks(document_id,ord,text,context,published_at)
SELECT id, 0, title || E'\n' || lead, source || ', ' || to_char(published_at AT TIME ZONE 'Europe/Moscow', 'YYYY-MM-DD'), published_at
FROM documents WHERE id LIKE 'rss:%';
DROP TABLE news_items;
