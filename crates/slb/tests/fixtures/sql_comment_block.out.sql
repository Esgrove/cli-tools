-- Seed the development database with the accounts the integration tests expect to find.
-- Test API clients:
--   client-one / api-secret-one
--   client-two / api-secret-two
INSERT INTO api_client (name, secret) VALUES ('client-one', 'api-secret-one');

-- The reader role keeps its own search path, so no explicit statement is needed here,
-- and the migration stays readable.
GRANT SELECT ON ALL TABLES IN SCHEMA public TO reader;

-- Backfill the new column with a default derived from the existing status column.
-- This runs once and is safe to re-run because the WHERE clause only touches rows that still have the old default.
UPDATE api_client SET tier = 'standard' WHERE tier IS NULL;

-- See https://example.com/migrations/0042 for the full rationale behind this column rename.
ALTER TABLE api_client RENAME COLUMN secret TO api_secret;
