-- #1494: COPY FROM fs9:// must check file privilege before table INSERT privilege.
--
-- PostgreSQL checks server-file read permission before table-level INSERT
-- privilege for COPY … FROM 'filename'.  Before the fix, db9 checked INSERT
-- first, leaking table-existence information to unprivileged users.
--
-- This test creates a non-superuser who DOES have INSERT on the target table
-- but is NOT a superuser, so the file-level check must fire first.

SET client_min_messages = warning;

DROP TABLE IF EXISTS t_copy_fs9_priv_1494;
DROP ROLE IF EXISTS r_copy_fs9_insert_1494;

CREATE TABLE t_copy_fs9_priv_1494 (id INT);
CREATE ROLE r_copy_fs9_insert_1494 LOGIN PASSWORD 'test';

-- Grant INSERT so the table-privilege check would pass if reached.
GRANT INSERT ON t_copy_fs9_priv_1494 TO r_copy_fs9_insert_1494;

SET ROLE r_copy_fs9_insert_1494;

-- File privilege must be denied before the INSERT privilege is even checked.
COPY t_copy_fs9_priv_1494 FROM 'fs9://any_file.csv';

RESET ROLE;

DROP TABLE t_copy_fs9_priv_1494;
DROP ROLE r_copy_fs9_insert_1494;
