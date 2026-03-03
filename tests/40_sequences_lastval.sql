-- T1: Fresh session — setval only → lastval errors (setval does NOT establish sentinel)
DROP SEQUENCE IF EXISTS lv_s1;
CREATE SEQUENCE lv_s1;
SELECT setval('lv_s1', 100);
SELECT lastval();

-- T2: nextval then setval(same seq) → lastval returns setval value (same_seq update)
DROP SEQUENCE IF EXISTS lv_s2;
CREATE SEQUENCE lv_s2;
SELECT nextval('lv_s2');
SELECT setval('lv_s2', 999);
SELECT lastval();

-- Cleanup
DROP SEQUENCE lv_s1;
DROP SEQUENCE lv_s2;
