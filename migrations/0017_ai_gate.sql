-- An AI portfolio passes the promotion gate by decisions of its model, not by
-- fills: a model may reasonably keep the money in cash.
ALTER TABLE live_settings ADD COLUMN min_ai_decisions INTEGER NOT NULL DEFAULT 5
    CHECK (min_ai_decisions BETWEEN 0 AND 1000);
