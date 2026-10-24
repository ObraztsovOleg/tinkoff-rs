-- Shared history cache: time ranges of an instrument's candles fully downloaded from
-- the broker. Complete candles of these ranges are in `candles`; processes read them
-- from here and ask the broker only for what is missing.
CREATE TABLE candle_coverage (
    instrument_id TEXT NOT NULL,
    interval TEXT NOT NULL,
    from_time TIMESTAMPTZ NOT NULL,
    to_time TIMESTAMPTZ NOT NULL CHECK (to_time > from_time)
);
CREATE INDEX candle_coverage_lookup_idx ON candle_coverage (instrument_id, interval, from_time);
