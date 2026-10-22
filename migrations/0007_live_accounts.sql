-- Real (production) accounts. Trading on them needs the operator's token secret
-- and TINKOFF_ALLOW_LIVE_TRADING=true in the environment; this table only keeps
-- the promotion gate, the emergency stop and the last read-only account survey.
CREATE TABLE live_settings (
    id BOOLEAN PRIMARY KEY DEFAULT true CHECK(id),
    halted BOOLEAN NOT NULL DEFAULT false,
    halted_at TIMESTAMPTZ,
    halt_reason TEXT,
    -- Sandbox evidence required before a portfolio may trade a real account.
    min_sessions INTEGER NOT NULL DEFAULT 5 CHECK (min_sessions BETWEEN 1 AND 60),
    min_fills INTEGER NOT NULL DEFAULT 1 CHECK (min_fills BETWEEN 0 AND 1000),
    require_no_risk_block BOOLEAN NOT NULL DEFAULT true,
    accounts JSONB NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(accounts) = 'array'),
    accounts_observed_at TIMESTAMPTZ,
    accounts_error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO live_settings(id) VALUES(true);

ALTER TABLE strategy_controls ADD COLUMN contour TEXT NOT NULL DEFAULT 'sandbox'
    CHECK (contour IN ('sandbox','production'));
-- A real account confirms every plan and never liquidates on a schedule.
ALTER TABLE strategy_controls ADD CONSTRAINT strategy_controls_live_confirms
    CHECK (contour = 'sandbox' OR execution_mode = 'confirm');
ALTER TABLE strategy_controls ADD CONSTRAINT strategy_controls_live_keeps_positions
    CHECK (contour = 'sandbox' OR NOT close_after);

-- account_risk_state keeps only the current block; the gate needs the history.
CREATE TABLE risk_block_events (
    account_id TEXT NOT NULL,
    strategy_id TEXT NOT NULL,
    blocked_at TIMESTAMPTZ NOT NULL,
    reason TEXT NOT NULL,
    detail TEXT,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (strategy_id, blocked_at, reason)
);
CREATE FUNCTION record_risk_block() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF jsonb_typeof(NEW.state->'blocked') = 'object' THEN
        INSERT INTO risk_block_events(account_id, strategy_id, blocked_at, reason, detail)
        VALUES (NEW.account_id, NEW.strategy_id,
                (NEW.state->'blocked'->>'blocked_at')::timestamptz,
                NEW.state->'blocked'->>'reason', NEW.state->'blocked'->>'detail')
        ON CONFLICT DO NOTHING;
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER account_risk_state_blocks AFTER INSERT OR UPDATE OF state ON account_risk_state
    FOR EACH ROW EXECUTE FUNCTION record_risk_block();
INSERT INTO risk_block_events(account_id, strategy_id, blocked_at, reason, detail)
SELECT account_id, strategy_id, (state->'blocked'->>'blocked_at')::timestamptz,
       state->'blocked'->>'reason', state->'blocked'->>'detail'
FROM account_risk_state WHERE jsonb_typeof(state->'blocked') = 'object'
ON CONFLICT DO NOTHING;
