-- Server identity + common driver introspection

SELECT version() LIKE 'PostgreSQL%' AS version_prefix;
SELECT version() LIKE '%pg-tikv%' AS version_has_pgtikv;

SELECT current_setting('server_version') AS server_version;
SELECT current_setting('server_version_num')::int AS server_version_num;

SHOW server_version_num;
SHOW TimeZone;
SHOW server_encoding;
SHOW DateStyle;
SHOW integer_datetimes;
SHOW IntervalStyle;
SHOW is_superuser;
SHOW session_authorization;

SELECT current_setting('server_encoding') AS server_encoding;
SELECT current_setting('DateStyle') AS datestyle;
SELECT current_setting('integer_datetimes') AS integer_datetimes;
SELECT current_setting('IntervalStyle') AS intervalstyle;
SELECT current_setting('is_superuser') AS is_superuser;
SELECT current_setting('session_authorization') AS session_authorization;

SET application_name = 'pg-tikv-tests';
SELECT current_setting('application_name') AS application_name;

SET TIME ZONE 'Asia/Shanghai';
SELECT current_setting('TimeZone') AS timezone;
