-- Runtime v2 intentionally does not recover unfinished v1 intents. Requiring
-- operators to settle them before migration prevents an old order from being
-- duplicated by a portfolio rebalance.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM trade_intents
        WHERE status IN ('pending', 'submitting', 'accepted', 'uncertain')
    ) THEN
        RAISE EXCEPTION
            'portfolio runtime migration refused: unfinished legacy trade_intents exist';
    END IF;
END
$$;

ALTER TABLE strategy_instances
    ADD COLUMN runtime_version SMALLINT NOT NULL DEFAULT 1,
    ADD COLUMN account_id TEXT;

ALTER TABLE strategy_instances
    ADD CONSTRAINT strategy_instances_v2_account CHECK (
        runtime_version <> 2 OR account_id IS NOT NULL
    );

ALTER TABLE strategy_state
    ADD COLUMN runtime_version SMALLINT NOT NULL DEFAULT 1;

CREATE TABLE rebalance_batches (
    id UUID PRIMARY KEY,
    event_id TEXT NOT NULL,
    strategy_id TEXT NOT NULL REFERENCES strategy_instances(id),
    account_id TEXT NOT NULL,
    target JSONB NOT NULL,
    effective_risk JSONB NOT NULL,
    sizing_capital NUMERIC(38, 9) NOT NULL CHECK (sizing_capital >= 0),
    status TEXT NOT NULL CHECK (
        status IN ('planned', 'reducing', 'opening', 'awaiting_fills', 'completed', 'failed')
    ),
    reason TEXT,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (strategy_id, event_id)
);

CREATE TABLE account_risk_state (
    account_id TEXT PRIMARY KEY,
    strategy_id TEXT NOT NULL REFERENCES strategy_instances(id),
    epoch UUID NOT NULL,
    state JSONB NOT NULL,
    runtime_version SMALLINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

ALTER TABLE trade_intents
    DROP CONSTRAINT IF EXISTS trade_intents_strategy_id_event_id_key;

ALTER TABLE trade_intents
    ADD COLUMN runtime_version SMALLINT NOT NULL DEFAULT 1,
    ADD COLUMN batch_id UUID REFERENCES rebalance_batches(id),
    ADD COLUMN leg_index INTEGER,
    ADD COLUMN target_lots BIGINT,
    ADD COLUMN lot_size BIGINT,
    ADD COLUMN execution_phase TEXT,
    ADD COLUMN margin_consent TEXT;

ALTER TABLE trade_intents
    ADD CONSTRAINT trade_intents_v2_shape CHECK (
        runtime_version <> 2 OR (
            batch_id IS NOT NULL
            AND leg_index IS NOT NULL
            AND leg_index >= 0
            AND target_lots IS NOT NULL
            AND lot_size IS NOT NULL
            AND lot_size > 0
            AND execution_phase IN ('reduction', 'opening')
            AND margin_consent IN ('covered_only', 'uncovered_allowed')
        )
    );

CREATE UNIQUE INDEX trade_intents_v2_batch_leg_key
    ON trade_intents (batch_id, leg_index)
    WHERE runtime_version = 2;

CREATE INDEX rebalance_batches_pending_idx
    ON rebalance_batches (status, created_at);

CREATE INDEX account_risk_state_strategy_idx
    ON account_risk_state (strategy_id);
