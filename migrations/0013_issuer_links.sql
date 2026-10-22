-- Links seen on issuer sites (press releases, reports). The first crawl of a site
-- only remembers its links; later new links become documents, so an old release
-- never looks like a fresh event.
CREATE TABLE issuer_links (
    url TEXT PRIMARY KEY,
    ticker TEXT NOT NULL,
    first_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    document_id TEXT
);
CREATE INDEX issuer_links_ticker_idx ON issuer_links(ticker);
