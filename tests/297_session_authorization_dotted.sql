-- #1564: dotted "session.authorization" must remain a normal custom GUC.
SET "session.authorization" = 'custom_val_1564';

SHOW "session.authorization";
SHOW session_authorization;

SELECT CASE
    WHEN current_setting('session.authorization') = 'custom_val_1564'
        THEN 'dotted_matches_set:OK'
    ELSE 'dotted_matches_set:FAIL'
END AS dotted_matches_set;
SELECT CASE
    WHEN current_setting('session_authorization') = 'custom_val_1564'
        THEN 'pseudo_matches_custom:FAIL'
    ELSE 'pseudo_matches_custom:OK'
END AS pseudo_matches_custom;
SELECT CASE
    WHEN current_setting('session.authorization') <> current_setting('session_authorization')
        THEN 'dotted_differs_from_pseudo:OK'
    ELSE 'dotted_differs_from_pseudo:FAIL'
END AS dotted_differs_from_pseudo;

-- session_authorization must equal current session user
SELECT CASE
    WHEN current_setting('session_authorization') = current_user
        THEN 'pseudo_is_current_user:OK'
    ELSE 'pseudo_is_current_user:FAIL'
END AS pseudo_is_current_user;
