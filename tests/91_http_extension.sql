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
    '{"message": "hello from db9-server"}',
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

-- Custom headers: pgsql-http array format
SELECT status,
       content::jsonb -> 'headers' ->> 'X-Test-Header' AS test_header
FROM extensions.http_get(
    'https://httpbin.org/get',
    '[{"field":"X-Test-Header","value":"hello-from-db9"}]'
);

-- Custom headers: object shorthand format
SELECT status,
       content::jsonb -> 'headers' ->> 'Authorization' AS auth_header
FROM extensions.http_get(
    'https://httpbin.org/get',
    '{"Authorization":"Bearer test-token-123"}'
);

-- Custom headers on http_post
SELECT status,
       content::jsonb -> 'headers' ->> 'X-Api-Key' AS api_key
FROM extensions.http_post(
    'https://httpbin.org/post',
    '{"data":"test"}',
    'application/json',
    '{"X-Api-Key":"sk-test-key"}'
);

-- Custom headers on http_put
SELECT status,
       content::jsonb -> 'headers' ->> 'X-Put-Header' AS put_header
FROM extensions.http_put(
    'https://httpbin.org/put',
    '{"data":"update"}',
    'application/json',
    '[{"field":"X-Put-Header","value":"put-value"}]'
);

-- Custom headers on http_delete
SELECT status,
       content::jsonb -> 'headers' ->> 'X-Delete-Token' AS del_token
FROM extensions.http_delete(
    'https://httpbin.org/delete',
    '{"X-Delete-Token":"del-123"}'
);

-- Universal http() function: GET
SELECT status,
       content::jsonb -> 'headers' ->> 'X-Universal' AS universal_hdr
FROM extensions.http(
    'GET',
    'https://httpbin.org/get',
    '{"X-Universal":"works"}'
);

-- Universal http() function: POST with body
SELECT status,
       content::jsonb -> 'json' ->> 'msg' AS msg,
       content::jsonb -> 'headers' ->> 'X-Custom' AS custom_hdr
FROM extensions.http(
    'POST',
    'https://httpbin.org/post',
    '{"X-Custom":"via-universal"}',
    'application/json',
    '{"msg":"universal-post"}'
);
