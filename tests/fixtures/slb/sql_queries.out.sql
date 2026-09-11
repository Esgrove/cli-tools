-- Collect the monthly totals for the report. The caller filters by the account identifier.
-- The query uses a window function, it needs at least version 8.0 of the server.
SELECT
    account_id,
    SUM(amount) AS total
FROM transactions
WHERE created_at >= '2026-01-01'
GROUP BY account_id;

-- A comment block that is already formatted correctly stays as it is.
CREATE INDEX idx_transactions_account ON transactions (account_id);
