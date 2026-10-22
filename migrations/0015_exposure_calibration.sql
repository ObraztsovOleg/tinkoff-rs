-- Measured reaction of instruments to market topics: on days with tagged events
-- of a topic, the return of the instrument above the Moscow Exchange index. A
-- suggestion for the operator; it never changes a sensitivity by itself.
CREATE TABLE exposure_suggestions (
    ticker TEXT NOT NULL,
    topic TEXT NOT NULL,
    observations INTEGER NOT NULL,
    effect_percent NUMERIC(10,4) NOT NULL,
    correlation NUMERIC(6,4),
    suggested NUMERIC(3,1) NOT NULL,
    computed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (ticker, topic)
);
