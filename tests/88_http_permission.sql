-- HTTP extension execution should be restricted to superusers by default.

CREATE EXTENSION IF NOT EXISTS http;

DROP ROLE IF EXISTS bob;
CREATE ROLE bob LOGIN PASSWORD 'bob';

\setenv PGPASSWORD bob
\connect postgres bob

SELECT status FROM extensions.http_get('https://example.com');
