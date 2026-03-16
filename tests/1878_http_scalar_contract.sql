-- HTTP scalar function contract (#1878)
-- Validates that http_get/http_post/etc. work as scalar functions
-- returning JSONB, in addition to the existing table-function syntax.

CREATE EXTENSION IF NOT EXISTS http;

-- 1. Basic scalar call returns JSONB with expected keys
SELECT
    http_get('https://httpbin.org/get') ? 'status' AS has_status,
    http_get('https://httpbin.org/get') ? 'content' AS has_content,
    http_get('https://httpbin.org/get') ? 'content_type' AS has_content_type,
    http_get('https://httpbin.org/get') ? 'headers' AS has_headers;

-- 2. Status extraction via JSONB operator
SELECT http_get('https://httpbin.org/get')->>'status' AS status;

-- 3. Content extraction and nested JSON parsing
SELECT (http_get('https://httpbin.org/get')->>'content')::json->>'url' AS url;

-- 4. http_post scalar
SELECT http_post('https://httpbin.org/post', '{"key":"val"}', 'application/json')->>'status' AS status;

-- 5. Table function syntax still works (regression guard)
SELECT status FROM extensions.http_get('https://httpbin.org/get');
