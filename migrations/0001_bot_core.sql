-- Неизменяемое описание скомпилированного экземпляра стратегии.
CREATE TABLE strategy_instances (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    config JSONB NOT NULL DEFAULT '{}'::jsonb,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Последнее подтверждённое изменяемое state. Обновляется в одной транзакции
-- с processed_events и созданными trade_intents.
CREATE TABLE strategy_state (
    strategy_id TEXT PRIMARY KEY REFERENCES strategy_instances(id) ON DELETE CASCADE,
    state JSONB NOT NULL,
    version BIGINT NOT NULL DEFAULT 1,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Durable deduplication key: одна стратегия применяет event_id только один раз.
CREATE TABLE processed_events (
    strategy_id TEXT NOT NULL REFERENCES strategy_instances(id) ON DELETE CASCADE,
    event_id TEXT NOT NULL,
    processed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (strategy_id, event_id)
);

-- Локальный OHLCV-кэш. Естественный ключ не позволяет дублировать одну свечу
-- после stream reconnect/redelivery.
CREATE TABLE candles (
    instrument_id TEXT NOT NULL,
    interval TEXT NOT NULL,
    open_time TIMESTAMPTZ NOT NULL,
    open NUMERIC(38, 9) NOT NULL,
    high NUMERIC(38, 9) NOT NULL,
    low NUMERIC(38, 9) NOT NULL,
    close NUMERIC(38, 9) NOT NULL,
    volume BIGINT NOT NULL,
    complete BOOLEAN NOT NULL,
    PRIMARY KEY (instrument_id, interval, open_time)
);

-- Решение стратегии до сетевой mutation. request_id генерируется и сохраняется
-- заранее, затем повторно используется при reconciliation.
CREATE TABLE trade_intents (
    id UUID PRIMARY KEY,
    request_id UUID NOT NULL UNIQUE,
    event_id TEXT NOT NULL,
    strategy_id TEXT NOT NULL REFERENCES strategy_instances(id),
    account_id TEXT NOT NULL,
    instrument_id TEXT NOT NULL,
    side TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
    kind TEXT NOT NULL CHECK (kind IN ('market', 'limit', 'best_price')),
    lots BIGINT NOT NULL CHECK (lots > 0),
    limit_price NUMERIC(38, 9),
    status TEXT NOT NULL,
    reason TEXT,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (strategy_id, event_id)
);

-- Ускоряет risk-проверку незавершённой заявки по счёту и инструменту.
CREATE INDEX trade_intents_pending_idx
    ON trade_intents (account_id, instrument_id, status);

-- Последнее известное broker-состояние одной заявки. Связь один-к-одному с
-- intent нужна для восстановления после timeout/restart.
CREATE TABLE orders (
    request_id UUID PRIMARY KEY,
    intent_id UUID NOT NULL UNIQUE REFERENCES trade_intents(id),
    exchange_order_id TEXT,
    status TEXT NOT NULL,
    lots_requested BIGINT NOT NULL,
    lots_executed BIGINT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Отдельные фактические исполнения. trade_id защищает от повторной stream-доставки.
CREATE TABLE fills (
    trade_id TEXT PRIMARY KEY,
    request_id UUID NOT NULL REFERENCES orders(request_id),
    price NUMERIC(38, 9) NOT NULL,
    lots BIGINT NOT NULL,
    executed_at TIMESTAMPTZ NOT NULL
);
