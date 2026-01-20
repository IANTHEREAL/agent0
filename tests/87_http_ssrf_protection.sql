-- SSRF protection should reject loopback IPs.

CREATE EXTENSION IF NOT EXISTS http;

SELECT status FROM extensions.http_get('https://127.0.0.1/');
SELECT status FROM extensions.http_get('https://[::ffff:127.0.0.1]/');

SELECT h.status
FROM extensions.http_get('https://127.0.0.1/') AS h
CROSS JOIN generate_series(1, 1) AS g(n);
