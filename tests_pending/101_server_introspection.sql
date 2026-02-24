-- Server identity + common driver introspection

SELECT version() LIKE 'PostgreSQL%' AS version_prefix;
SELECT version() LIKE '%db9-server%' AS version_has_db9;

SELECT current_setting('server_version') AS server_version;
SELECT current_setting('server_version_num')::int AS server_version_num;

SHOW server_version_num;
SHOW TimeZone;

SET application_name = 'db9-server-tests';
SELECT current_setting('application_name') AS application_name;

SET TIME ZONE 'Asia/Shanghai';
SELECT current_setting('TimeZone') AS timezone;

