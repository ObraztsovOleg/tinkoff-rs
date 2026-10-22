-- The operator decides when each account trades and how many runtimes run at once.
ALTER TABLE dashboard_settings ADD COLUMN max_runtimes INTEGER NOT NULL DEFAULT 4
    CHECK (max_runtimes BETWEEN 1 AND 10);
-- schedule: trade only on trading_days (warm-up one hour before the session);
-- always: trade whenever enabled. trading_days holds MOEX dates (YYYY-MM-DD).
ALTER TABLE strategy_controls ADD COLUMN run_mode TEXT NOT NULL DEFAULT 'schedule'
    CHECK (run_mode IN ('schedule','always'));
ALTER TABLE strategy_controls ADD COLUMN trading_days JSONB NOT NULL DEFAULT '[]'
    CHECK (jsonb_typeof(trading_days) = 'array');
-- Close positions after each scheduled window (the former campaign behaviour).
ALTER TABLE strategy_controls ADD COLUMN close_after BOOLEAN NOT NULL DEFAULT true;
-- False until the supervisor has converted the fixed campaign roster once.
ALTER TABLE strategy_controls ADD COLUMN schedule_initialized BOOLEAN NOT NULL DEFAULT false;
