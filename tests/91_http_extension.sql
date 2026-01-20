CREATE EXTENSION IF NOT EXISTS http;

SELECT status, content_type IS NOT NULL AS has_content_type, LENGTH(content) > 0 AS has_content
FROM extensions.http_get('https://httpbin.org/get');

SELECT status, 
       content::jsonb -> 'args' ->> 'foo' AS foo_param
FROM extensions.http_get('https://httpbin.org/get?foo=bar');

SELECT status,
       content::jsonb -> 'json' ->> 'message' AS posted_message
FROM extensions.http_post(
    'https://httpbin.org/post',
    '{"message": "hello from pg-tikv"}',
    'application/json'
);

SELECT status,
       content::jsonb -> 'form' ->> 'name' AS form_name
FROM extensions.http_post(
    'https://httpbin.org/post',
    'name=test&value=123',
    'application/x-www-form-urlencoded'
);

SELECT status, content_type IS NOT NULL AS has_content_type, LENGTH(content) = 0 AS empty_content
FROM extensions.http_head('https://httpbin.org/get');

SELECT status,
       content::jsonb -> 'json' ->> 'updated' AS updated_value
FROM extensions.http_put(
    'https://httpbin.org/put',
    '{"updated": "true"}',
    'application/json'
);

SELECT status
FROM extensions.http_delete('https://httpbin.org/delete');

SELECT status,
       jsonb_typeof(headers::jsonb) AS headers_type
FROM extensions.http_get('https://httpbin.org/get');

SELECT status
FROM extensions.http_get('https://httpbin.org/status/404');

SELECT status
FROM extensions.http_get('https://httpbin.org/status/500');

SELECT status
FROM extensions.http_get('https://httpbin.org/redirect-to?url=https%3A%2F%2Fhttpbin.org%2Fget');

SELECT (content::jsonb -> 'headers') IS NOT NULL AS has_headers
FROM extensions.http_get('https://httpbin.org/get');

SELECT (SELECT status FROM extensions.http_get('https://httpbin.org/get')) AS subquery_status;

SELECT 
    (SELECT status FROM extensions.http_get('https://httpbin.org/status/200')) AS status_200,
    (SELECT status FROM extensions.http_get('https://httpbin.org/status/201')) AS status_201;
