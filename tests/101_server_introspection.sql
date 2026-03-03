-- Server identity + common driver introspection

SELECT version() LIKE 'PostgreSQL%' AS version_prefix;
-- db9-specific: db9 version string includes db9-server; PG returns f for this predicate
SELECT version() LIKE '%db9-server%' AS version_has_db9;

-- db9-specific: db9 reports server_version as 16.0 for compatibility
SELECT current_setting('server_version') AS server_version;
-- db9-specific: db9 reports compatibility server_version_num=160000
SELECT current_setting('server_version_num')::int AS server_version_num;

-- db9-specific: SHOW server_version_num is 160000 for compatibility
SHOW server_version_num;
SHOW TimeZone;
SHOW server_encoding;
SHOW DateStyle;
SHOW integer_datetimes;
SHOW IntervalStyle;
SHOW search_path;
SHOW default_transaction_isolation;
SHOW is_superuser;
-- db9-specific: session_authorization is fixed to admin in db9
SHOW session_authorization;

SELECT current_setting('server_encoding') AS server_encoding;
SELECT current_setting('DateStyle') AS datestyle;
SELECT current_setting('integer_datetimes') AS integer_datetimes;
SELECT current_setting('IntervalStyle') AS intervalstyle;
SELECT current_setting('search_path') AS search_path;
SELECT current_setting('default_transaction_isolation') AS default_transaction_isolation;
SELECT current_setting('is_superuser') AS is_superuser;
-- db9-specific: session_authorization is fixed to admin in db9
SELECT current_setting('session_authorization') AS session_authorization;

SET application_name = 'db9-server-tests';
SELECT current_setting('application_name') AS application_name;

SET TIME ZONE 'Asia/Shanghai';
SELECT current_setting('TimeZone') AS timezone;
