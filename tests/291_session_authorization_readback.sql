-- #1560: dotted "session.authorization" must read back the stored GUC value.
SET "session.authorization" = '__db9_issue_1560__';

SHOW "session.authorization";
SELECT current_setting('session.authorization') AS dotted_readback;

SHOW session_authorization;
SELECT current_setting('session_authorization') AS pseudo_readback;

SELECT current_setting('session.authorization') = '__db9_issue_1560__' AS dotted_matches_set;
SELECT
    current_setting('session.authorization') = current_setting('session_authorization')
        AS dotted_equals_pseudo;

-- Negative control for .errors matching.
SHOW definitely_missing_session_authorization_setting_291;
