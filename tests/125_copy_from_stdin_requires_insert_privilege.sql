-- Issue #687: COPY FROM STDIN must enforce INSERT privilege before entering copy mode.

SET client_min_messages = warning;

DROP TABLE IF EXISTS t_copy_rbac;
DROP ROLE IF EXISTS copy_rbac_user;

CREATE TABLE t_copy_rbac (id INT PRIMARY KEY);
CREATE ROLE copy_rbac_user LOGIN PASSWORD 'pw';

-- No privileges granted; COPY should be rejected before reading any data lines.
SET ROLE copy_rbac_user;
COPY t_copy_rbac (id) FROM STDIN;
RESET ROLE;

DROP TABLE t_copy_rbac;
DROP ROLE copy_rbac_user;

