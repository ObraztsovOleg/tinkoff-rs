-- Commission per contour and instrument type. The supervisor sets the tariff
-- default (UsersService/GetInfo) and replaces it with the rate measured from the
-- commissions the broker actually charged once there are enough trades.
CREATE TABLE commission_rates (
    contour TEXT NOT NULL CHECK (contour IN ('sandbox', 'production')),
    kind TEXT NOT NULL,
    rate_percent NUMERIC(8,5) NOT NULL CHECK (rate_percent >= 0 AND rate_percent < 5),
    source TEXT NOT NULL CHECK (source IN ('tariff', 'measured')),
    tariff TEXT,
    trades INTEGER NOT NULL DEFAULT 0,
    volume_rub NUMERIC(20,2) NOT NULL DEFAULT 0,
    commission_rub NUMERIC(20,2) NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (contour, kind)
);
-- Position changes smaller than this share of the strategy capital are not traded
-- (closing a position and reversing it always are): small rebalances only pay commission.
ALTER TABLE dashboard_settings ADD COLUMN min_trade_percent NUMERIC(6,3) NOT NULL DEFAULT 0.5
    CHECK (min_trade_percent >= 0 AND min_trade_percent <= 20);
-- Commission of the decision's turnover; portfolio_return is net of it.
ALTER TABLE ai_outcomes ADD COLUMN costs_percent NUMERIC(10,4);
