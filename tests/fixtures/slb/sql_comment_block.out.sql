-- Seed the development database with the accounts the integration tests expect to find.
-- Test API clients:
--   client-one / api-secret-one
--   client-two / api-secret-two
INSERT INTO api_client (name, secret) VALUES ('client-one', 'api-secret-one');

-- The reader role keeps its own search path, so no explicit statement is needed here,
-- and the migration stays readable.
GRANT SELECT ON ALL TABLES IN SCHEMA public TO reader;
