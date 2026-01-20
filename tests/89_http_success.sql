-- Happy path smoke test (requires outbound internet access).

CREATE EXTENSION IF NOT EXISTS http;

SELECT status FROM extensions.http_get('https://example.com');

