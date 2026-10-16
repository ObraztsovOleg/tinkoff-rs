-- Full portfolio targets are mutually exclusive: while one batch can still
-- change positions, another batch for the same dedicated account is unsafe,
-- even if a misconfigured process presents another strategy_id.
-- If an older runtime already created several unfinished batches, migration
-- intentionally stops so the operator can reconcile them before continuing.
DO $$
BEGIN
    IF EXISTS (
        SELECT account_id
        FROM rebalance_batches
        WHERE status NOT IN ('completed', 'failed')
        GROUP BY account_id
        HAVING count(*) > 1
    ) THEN
        RAISE EXCEPTION
            'single-flight migration refused: an account has several unfinished rebalance batches';
    END IF;
END
$$;

CREATE UNIQUE INDEX rebalance_batches_one_unfinished_per_account
    ON rebalance_batches (account_id)
    WHERE status NOT IN ('completed', 'failed');
