-- Control requests are durable; only the account lease holder executes broker writes.
CREATE TABLE dashboard_settings (
    id BOOLEAN PRIMARY KEY DEFAULT true CHECK(id),
    mode TEXT NOT NULL DEFAULT 'campaign' CHECK(mode IN ('campaign','free')),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO dashboard_settings(id) VALUES(true);

-- Deliberately no strategy FK: scheduled scenarios may not be registered yet.
CREATE TABLE strategy_controls (
    strategy_id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT true,
    execution_mode TEXT NOT NULL DEFAULT 'auto' CHECK(execution_mode IN ('auto','confirm')),
    execution_budget NUMERIC(38,9) CHECK(execution_budget > 0),
    revision BIGINT NOT NULL DEFAULT 0,
    needs_service BOOLEAN NOT NULL DEFAULT false,
    cleanup_requested BOOLEAN NOT NULL DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE control_commands (
    id UUID PRIMARY KEY,
    strategy_id TEXT,
    account_id TEXT,
    kind TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'queued' CHECK(status IN ('queued','running','succeeded','failed','uncertain')),
    result JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX control_commands_pending_idx ON control_commands(account_id,created_at) WHERE status IN ('queued','running');
CREATE TABLE batch_controls (
    batch_id UUID PRIMARY KEY REFERENCES rebalance_batches(id),
    status TEXT NOT NULL CHECK(status IN ('awaiting','approved','rejected')),
    revision BIGINT NOT NULL,
    preview JSONB NOT NULL DEFAULT '{}',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE instrument_catalog (
    uid TEXT PRIMARY KEY,
    metadata JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE account_managed_instruments (
    account_id TEXT NOT NULL,
    instrument_id TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    PRIMARY KEY(account_id,instrument_id)
);
CREATE TABLE strategy_runs (
    id UUID PRIMARY KEY,
    strategy_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    classification TEXT NOT NULL CHECK(classification IN ('clean','mixed')),
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at TIMESTAMPTZ,
    anchor_equity NUMERIC(38,9) NOT NULL,
    principal NUMERIC(38,9) NOT NULL CHECK(principal > 0),
    anchor_external_cash_flows NUMERIC(38,9) NOT NULL,
    last_equity NUMERIC(38,9) NOT NULL,
    last_external_cash_flows NUMERIC(38,9) NOT NULL,
    peak_pnl NUMERIC(38,9) NOT NULL DEFAULT 0,
    max_drawdown_percent NUMERIC(38,9) NOT NULL DEFAULT 0,
    interventions JSONB NOT NULL DEFAULT '[]',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX strategy_runs_active_idx ON strategy_runs(account_id) WHERE ended_at IS NULL;
CREATE TABLE performance_samples (
    run_id UUID NOT NULL REFERENCES strategy_runs(id),
    observed_at TIMESTAMPTZ NOT NULL,
    equity NUMERIC(38,9) NOT NULL,
    external_cash_flows NUMERIC(38,9) NOT NULL,
    PRIMARY KEY(run_id,observed_at)
);
