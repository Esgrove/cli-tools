-- Collect the monthly totals for the report. The caller filters by the account identifier.
-- The query uses a window function. It needs at least version 8.0 of the server.
SELECT
    account_id,
    SUM(amount) AS total
FROM transactions
WHERE created_at >= '2026-01-01'
GROUP BY account_id;

-- A comment block that is already formatted correctly stays as it is.
CREATE INDEX idx_transactions_account ON transactions (account_id);

-- Ranks every account by its total spend within the month and keeps only the top ten.
-- Ties are broken by account identifier so the result is stable across runs on the same data.
SELECT account_id, total, RANK() OVER (ORDER BY total DESC, account_id) AS rank
FROM (
    SELECT account_id, SUM(amount) AS total
    FROM transactions
    GROUP BY account_id
) AS totals
WHERE rank <= 10;

-- See https://example.com/schema/transactions for the full column reference and constraints.
COMMENT ON TABLE transactions IS 'One row per completed payment; refunds are separate rows.';
