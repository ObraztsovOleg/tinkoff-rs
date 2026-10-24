-- Each trade the broker reports with the commission it charged: the supervisor
-- copies them from the operations of the stand's accounts, so the dashboard shows
-- what every portfolio paid over its run.
CREATE TABLE trade_commissions (
    operation_id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    executed_at TIMESTAMPTZ NOT NULL,
    kind TEXT NOT NULL,
    volume_rub NUMERIC(20,2) NOT NULL CHECK (volume_rub >= 0),
    commission_rub NUMERIC(20,2) NOT NULL CHECK (commission_rub >= 0)
);
CREATE INDEX trade_commissions_account_idx ON trade_commissions(account_id, executed_at);
