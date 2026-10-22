-- AI spend in rubles: the USD/RUB rate the supervisor reads from T-Invest, the ruble
-- cost of each AI job and decision, and the daily AI budget set from the dashboard.
CREATE TABLE fx_rates (
    currency TEXT PRIMARY KEY,
    rub NUMERIC(14,6) NOT NULL CHECK (rub > 0),
    source TEXT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE ai_usage ADD COLUMN cost_rub NUMERIC(14,4);
ALTER TABLE ai_decisions ADD COLUMN cost_rub NUMERIC(14,4);
-- Spend on model requests per Moscow day; above it out-of-schedule decisions,
-- document tagging and reviews wait for the next day (scheduled decisions go on).
ALTER TABLE dashboard_settings ADD COLUMN ai_budget_rub NUMERIC(12,2) NOT NULL DEFAULT 500
    CHECK (ai_budget_rub > 0 AND ai_budget_rub <= 1000000);
