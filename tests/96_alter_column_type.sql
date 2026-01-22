-- PostgreSQL compatible tests from alter_column_type
-- 369 tests

SET client_min_messages = warning;

-- Test 1: statement (line 9)
SET TIME ZONE 'Europe/Amsterdam';

CREATE TABLE t (s TEXT, sl VARCHAR(5), t TIME, ts TIMESTAMP);

-- Test 2: query (line 15)
SELECT * FROM t;

-- Test 3: query (line 24)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't'
ORDER BY ordinal_position;

-- Test 4: query (line 34)
SELECT * FROM t;

-- Test 5: statement (line 39)
DROP TABLE IF EXISTS t;

-- Test 6: statement (line 49)
CREATE TABLE t (a INT, b TEXT);

-- Test 6: statement (line 49)
CREATE INDEX idx ON t (b);

-- Test 7: statement (line 52)
INSERT INTO t VALUES (1, '01'), (2, '002'), (3, '0003');

-- Test 8: query (line 55)
SELECT * from t ORDER BY b DESC;

-- Test 9: statement (line 63)
ALTER TABLE t ADD COLUMN i INT GENERATED ALWAYS AS (b::INT) STORED;

-- Test 10: statement (line 66)
CREATE INDEX idx2 ON t (i);

-- Test 11: statement (line 69)
ALTER TABLE t ALTER COLUMN i DROP EXPRESSION;

-- Test 12: statement (line 72)
ALTER TABLE t DROP COLUMN b CASCADE;

-- onlyif config schema-locked-disabled

-- Test 13: query (line 76)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't'
ORDER BY ordinal_position;

-- Test 14: query (line 88)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't'
ORDER BY ordinal_position;

-- Test 15: statement (line 99)
ALTER TABLE t RENAME COLUMN i TO b;

-- Test 16: statement (line 102)
ALTER INDEX idx2 RENAME TO idx;

-- Test 17: query (line 105)
SELECT * from t ORDER BY b DESC;

-- Test 18: statement (line 113)
DROP TABLE IF EXISTS t CASCADE;

-- Test 19: statement (line 120)
CREATE TABLE t (a INT);

-- Test 20: query (line 123)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't'
ORDER BY ordinal_position;

-- Test 21: statement (line 130)
ALTER TABLE t ALTER COLUMN a TYPE INTEGER;

-- Test 22: query (line 133)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't'
ORDER BY ordinal_position;

-- Test 23: statement (line 140)
DROP TABLE IF EXISTS t;

-- Test 24: statement (line 153)
DROP TABLE IF EXISTS t;

-- Test 25: statement (line 160)
CREATE TABLE t (i INT);

-- Test 26: statement (line 163)
ALTER TABLE t ALTER COLUMN i TYPE INT;

-- Test 27: statement (line 166)
DROP TABLE IF EXISTS t;

-- Test 28: statement (line 179)
DROP TABLE IF EXISTS t;

-- Test 29: statement (line 188)
CREATE TABLE t1 (date DATE);

-- Test 29: statement (line 188)
INSERT INTO t1 VALUES ('2024-07-26');

-- Test 30: statement (line 195)
ALTER TABLE t1 ALTER COLUMN date TYPE timestamp;

-- Test 31: statement (line 198)
ALTER TABLE t1 ALTER COLUMN date TYPE timestamp USING date::TIMESTAMP;

-- Test 32: statement (line 202)
CREATE TABLE t2 (id int);

-- Test 33: statement (line 205)
INSERT INTO t2 VALUES (1), (2), (3), (4);

-- Test 34: query (line 211)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't2'
ORDER BY ordinal_position;

-- Test 35: statement (line 217)
INSERT INTO t2 VALUES ('5');

-- Test 36: statement (line 222)
CREATE TABLE t3 (id int, id2 int, id3 int);

-- Test 37: statement (line 225)
INSERT INTO t3 VALUES (1,1,1), (2,2,2), (3,3,3);

-- Test 38: query (line 231)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't3'
ORDER BY ordinal_position;

-- Test 39: query (line 240)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't3'
ORDER BY ordinal_position;

-- Test 40: query (line 254)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't3'
ORDER BY ordinal_position;

-- Test 41: statement (line 267)
INSERT INTO t3 VALUES (4,'4',4);

-- Test 42: query (line 270)
SELECT * FROM t3 ORDER BY id;

-- Test 43: statement (line 279)
CREATE TABLE t5 (x TIMESTAMPTZ(6));

-- Test 44: statement (line 282)
INSERT INTO t5 VALUES ('2016-01-25 10:10:10.555555-05:00');

-- Test 45: statement (line 285)
INSERT INTO t5 VALUES ('2016-01-26 10:10:10.555555-05:00');

-- Test 46: statement (line 288)
ALTER TABLE t5 ALTER COLUMN x TYPE TIMESTAMPTZ(3);

-- Test 47: statement (line 291)
INSERT INTO t5 VALUES ('2016-01-26 10:10:10.55-05:00');

-- Test 48: query (line 294)
SELECT * FROM t5 ORDER BY x;

-- Test 49: statement (line 302)
CREATE TABLE t6(id INT, id2 INT);

-- Test 50: statement (line 305)
INSERT INTO t6 VALUES (1), (2), (3);

-- Test 51: query (line 311)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't6'
ORDER BY ordinal_position;

-- Test 52: statement (line 319)
CREATE TABLE t7 (x INT DEFAULT 1, y INT);

-- Test 53: statement (line 322)
INSERT INTO t7 (y) VALUES (1), (2), (3);

-- Test 54: statement (line 325)
ALTER TABLE t7 ALTER COLUMN x DROP DEFAULT;

-- Test 54: statement (line 325)
ALTER TABLE t7 ALTER COLUMN x TYPE DATE USING (DATE '1970-01-01' + x);

-- Test 55: statement (line 328)
ALTER TABLE t7 ALTER COLUMN x TYPE DATE;

-- Test 56: statement (line 335)
CREATE TABLE t8 (x TEXT);

-- Test 56: statement (line 335)
INSERT INTO t8 VALUES ('123');

-- Test 57: statement (line 338)
ALTER TABLE t8 ALTER COLUMN x TYPE INT USING x::BIGINT;

-- Test 58: statement (line 341)
ALTER TABLE t8 ALTER COLUMN x TYPE INT;

-- onlyif config schema-locked-disabled

-- Test 59: query (line 345)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't8'
ORDER BY ordinal_position;

-- Test 60: query (line 355)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't8'
ORDER BY ordinal_position;

-- Test 61: statement (line 365)
CREATE TABLE t9 (x INT PRIMARY KEY);

-- Test 62: statement (line 372)
CREATE TABLE t10 (x INT, y INT);

-- Test 62: statement (line 372)
CREATE INDEX ON t10 (x, y);

-- Test 63: statement (line 379)
CREATE TABLE t11 (x INT);

-- Test 64: statement (line 383)
CREATE TABLE t12 (x INT check (x > 0));

-- Test 65: statement (line 392)
CREATE TABLE uniq (x INT, y INT, UNIQUE (x, y));

-- Test 66: statement (line 399)
CREATE TABLE t15 (x INT, y INT);

-- Test 67: statement (line 402)
CREATE INDEX ON t15 (x) INCLUDE (y);

-- Test 68: statement (line 405)
INSERT INTO t15 VALUES (1, 1), (2, 2);

-- Test 69: statement (line 415)
CREATE TABLE t16 (x TEXT);

-- Test 69: statement (line 415)
INSERT INTO t16 VALUES ('Backhaus'), ('Bär'), ('Baz');

-- Test 70: query (line 418)
SELECT x FROM t16 ORDER BY x;

-- Test 71: query (line 428)
SELECT x FROM t16 ORDER BY x;

-- Test 72: statement (line 439)
CREATE TABLE t17 (x TEXT, y TEXT);

-- Test 72: statement (line 439)
ALTER TABLE t17 ALTER COLUMN x TYPE INT USING x::BIGINT;

-- Test 73: statement (line 442)
ALTER TABLE t17 ALTER COLUMN x TYPE INT;

-- Test 74: statement (line 445)
ALTER TABLE t17 ALTER COLUMN y TYPE INT USING y::BIGINT;

-- Test 75: statement (line 448)
ALTER TABLE t17 ALTER COLUMN y TYPE INT;

-- onlyif config schema-locked-disabled

-- Test 76: query (line 452)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't17'
ORDER BY ordinal_position;

-- Test 77: query (line 465)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't17'
ORDER BY ordinal_position;

-- Test 78: statement (line 478)
CREATE TABLE t18 (x INT NOT NULL PRIMARY KEY);

-- Test 79: statement (line 481)
CREATE TABLE t19 (y INT NOT NULL REFERENCES t18 (x));

-- Test 79: statement (line 481)
CREATE INDEX ON t19 (y);

-- Test 80: statement (line 491)
CREATE TABLE t20 (x INT);

-- Test 81: statement (line 494)
BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE;

-- Test 82: statement (line 497)
-- SET LOCAL autocommit_before_ddl=off;

-- Test 83: statement (line 503)
ROLLBACK;

-- Test 84: statement (line 507)
CREATE TABLE t21 (x INT);

-- Test 85: statement (line 510)
INSERT INTO t21 VALUES (888),(-32760);

-- Test 86: statement (line 516)
ALTER TABLE t21 ALTER COLUMN x TYPE VARCHAR(30);

-- Test 87: statement (line 519)
ALTER TABLE t21 ADD COLUMN y BIGINT, ALTER COLUMN x SET DATA TYPE TEXT;

-- Test 88: query (line 522)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't21'
ORDER BY ordinal_position;

-- Test 89: statement (line 528)
CREATE TABLE t22 (x INT);

-- Test 90: statement (line 531)
INSERT INTO t22 VALUES (0),(-5);

-- Test 91: query (line 537)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't22'
ORDER BY ordinal_position;

-- Test 92: statement (line 544)
CREATE TABLE t23 (x INT);
INSERT INTO t23 VALUES (-3), (-2), (-1), (0), (1), (2), (3);

-- Test 93: statement (line 548)
ALTER TABLE t23 ALTER COLUMN x TYPE BOOLEAN USING (x > 0);

-- Test 94: query (line 551)
SELECT x FROM t23 ORDER BY x;

-- Test 95: statement (line 566)
CREATE TABLE t24 (x TEXT);

-- Test 95: statement (line 566)
INSERT INTO t24 VALUES ('1'), ('2');

-- Test 96: statement (line 569)
ALTER TABLE t24  ALTER COLUMN x TYPE INT USING (x::int + 5);

-- onlyif config schema-locked-disabled

-- Test 97: query (line 573)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't24'
ORDER BY ordinal_position;

-- Test 98: query (line 584)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't24'
ORDER BY ordinal_position;

-- Test 99: statement (line 596)
CREATE TABLE t25 (x INT);
INSERT INTO t25 VALUES (1);

-- Test 100: statement (line 603)
CREATE TABLE t26 (x INT);

-- Test 101: statement (line 606)
CREATE TABLE t27 (x INT);

-- Test 102: statement (line 611)
ALTER TABLE t26 ALTER COLUMN x TYPE BOOLEAN USING (x > 0);

-- Test 103: statement (line 615)
ALTER TABLE t27 ALTER COLUMN x TYPE BOOLEAN USING (x > 0);

-- Test 104: statement (line 632)
CREATE TABLE t28(x INT);

-- Test 105: statement (line 635)
INSERT INTO t28 VALUES (1), (2), (3);

-- Test 106: statement (line 638)
ALTER TABLE t28 ALTER COLUMN x TYPE INT USING (x * 5);

-- Test 107: query (line 641)
SELECT x FROM t28 ORDER BY x;

-- Test 108: statement (line 650)
CREATE TABLE t29 (x BIGINT);

-- Test 109: statement (line 653)
INSERT INTO t29 VALUES (1), (2), (3);

-- Test 110: statement (line 656)
ALTER TABLE t29 ALTER COLUMN x TYPE INTEGER;

-- Test 111: query (line 659)
SELECT x FROM t29 ORDER BY x;

-- Test 112: statement (line 668)
CREATE TABLE parent_71089 (id INT PRIMARY KEY);

-- Test 113: statement (line 671)
CREATE TABLE child_71089 (a INT, b INT REFERENCES parent_71089 (id) NOT NULL);

-- Test 114: statement (line 674)
ALTER TABLE child_71089 ALTER COLUMN a TYPE FLOAT;

-- Test 115: statement (line 683)
CREATE TABLE t30 (x TEXT);

-- Test 115: statement (line 683)
INSERT INTO t30 VALUES (e'a\\01');

-- Test 116: statement (line 686)
ALTER TABLE t30 ALTER COLUMN x TYPE BYTEA USING convert_to(x, 'UTF8');

-- Test 117: statement (line 689)
ALTER TABLE t30 ALTER COLUMN x TYPE BYTEA;

-- Test 118: statement (line 694)
CREATE VIEW v AS SELECT x FROM t29;

-- Test 119: statement (line 697)
DROP VIEW v;

-- Test 120: statement (line 700)
ALTER TABLE t29 ALTER COLUMN x TYPE SMALLINT;

-- Test 121: statement (line 703)
CREATE MATERIALIZED VIEW v AS SELECT x FROM t29;

-- Test 122: statement (line 706)
DROP MATERIALIZED VIEW v;

-- Test 122: statement (line 706)
ALTER TABLE t29 ALTER COLUMN x TYPE SMALLINT;

-- Test 123: statement (line 709)
CREATE TABLE regression_54844 (i BIGINT);

-- Test 124: statement (line 712)
INSERT INTO regression_54844 VALUES (-9223372036854775807);

-- Test 125: statement (line 715)
UPDATE regression_54844 SET i = -1;

-- Test 125: statement (line 715)
ALTER TABLE regression_54844 ALTER COLUMN i TYPE SMALLINT;

-- Test 126: statement (line 721)
CREATE TABLE t_91069 (i INT PRIMARY KEY, j VARCHAR(64) NULL);

-- Test 127: statement (line 724)
ALTER TABLE t_91069 ALTER COLUMN j SET DEFAULT NULL;

-- Test 128: statement (line 727)
ALTER TABLE t_91069 ALTER COLUMN j TYPE VARCHAR(32);

-- Test 129: statement (line 733)
CREATE TABLE t31 (b BOOLEAN);

-- Test 130: statement (line 736)
INSERT INTO t31 VALUES (true),(false);

-- Test 131: statement (line 739)
ALTER TABLE t31 ALTER COLUMN b TYPE INT USING (CASE WHEN b THEN 1 ELSE 0 END);

-- Test 132: statement (line 742)
ALTER TABLE t31 ALTER COLUMN b TYPE INT;

-- Test 133: statement (line 747)
CREATE TABLE tab_w_computed (real1 int, real2 int, comp1 bigint, comp2 bigint, comp3 int);
INSERT INTO tab_w_computed (real1, real2) VALUES (1, 2), (3, 4), (5, 6);

-- Test 134: statement (line 751)
ALTER TABLE tab_w_computed ALTER COLUMN real1 TYPE bigint;

-- Test 135: statement (line 754)
ALTER TABLE tab_w_computed DROP COLUMN comp1;

-- Test 136: statement (line 757)
ALTER TABLE tab_w_computed ALTER COLUMN real1 TYPE bigint;

-- Test 137: statement (line 760)
ALTER TABLE tab_w_computed DROP COLUMN comp3;

-- Test 138: statement (line 763)
ALTER TABLE tab_w_computed ALTER COLUMN real1 TYPE bigint;

-- Test 139: statement (line 769)
ALTER TABLE tab_w_computed DROP COLUMN comp2;

-- Test 140: statement (line 775)
DROP TABLE IF EXISTS tab_w_computed;

-- Test 141: statement (line 780)
CREATE TABLE t_with_hidden (vis1 int, hid1 SMALLINT default 1);

-- Test 142: statement (line 783)
INSERT INTO t_with_hidden VALUES (0),(1),(2);

-- Test 143: query (line 786)
SELECT * FROM t_with_hidden ORDER BY vis1;

-- Test 144: statement (line 796)
INSERT INTO t_with_hidden VALUES (3);

-- Test 145: query (line 799)
SELECT * FROM t_with_hidden ORDER BY vis1;

-- Test 146: query (line 807)
SELECT vis1, hid1 FROM t_with_hidden ORDER BY vis1;

-- Test 147: query (line 816)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_with_hidden'
ORDER BY ordinal_position;

-- Test 148: query (line 828)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_with_hidden'
ORDER BY ordinal_position;

-- Test 149: statement (line 839)
DROP TABLE IF EXISTS t_with_hidden;

-- Test 150: statement (line 844)
CREATE TABLE t_with_on_update(a int primary key, b int DEFAULT 3);

-- Test 150: statement (line 844)
CREATE OR REPLACE FUNCTION t_with_on_update_set_b()
RETURNS trigger AS $$
BEGIN
    NEW.b := 3;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Test 150: statement (line 844)
CREATE TRIGGER t_with_on_update_set_b
BEFORE UPDATE ON t_with_on_update
FOR EACH ROW EXECUTE FUNCTION t_with_on_update_set_b();

-- Test 151: statement (line 847)
INSERT INTO t_with_on_update (a) VALUES (1);

-- Test 152: query (line 850)
SELECT a, b FROM t_with_on_update;

-- Test 153: statement (line 855)
UPDATE t_with_on_update SET a=a+1 WHERE a > 0;

-- Test 154: query (line 858)
SELECT a, b FROM t_with_on_update ORDER BY a;

-- Test 155: statement (line 863)
ALTER TABLE t_with_on_update ALTER COLUMN b SET DATA TYPE INTEGER;

-- Test 156: statement (line 866)
INSERT INTO t_with_on_update (a) VALUES (10);
UPDATE t_with_on_update SET a=a+1 WHERE a=10;
INSERT INTO t_with_on_update (a) VALUES (100);

-- Test 157: query (line 871)
SELECT a, b FROM t_with_on_update ORDER BY a;

-- Test 158: statement (line 881)
INSERT INTO t_with_on_update (a) VALUES (1000);
UPDATE t_with_on_update SET a=a+1 WHERE a=1000;
INSERT INTO t_with_on_update (a) VALUES (10000);

-- Test 159: query (line 886)
SELECT a, b FROM t_with_on_update ORDER BY a;

-- Test 160: query (line 896)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_with_on_update'
ORDER BY ordinal_position;

-- Test 161: query (line 907)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_with_on_update'
ORDER BY ordinal_position;

-- Test 162: statement (line 917)
DROP TABLE IF EXISTS t_with_on_update;

-- Test 163: statement (line 922)
CREATE TABLE t_with_default(c1 INTEGER default 1);

-- Test 164: statement (line 927)
ALTER TABLE t_with_default ALTER COLUMN c1 SET DATA TYPE SMALLINT;

-- Test 165: statement (line 930)
INSERT INTO t_with_default VALUES (default);

-- Test 166: statement (line 933)
ALTER TABLE t_with_default ALTER COLUMN c1 SET DEFAULT 32767;

-- Test 167: statement (line 936)
INSERT INTO t_with_default VALUES (default);

-- Test 168: query (line 939)
SELECT * FROM t_with_default ORDER BY c1;

-- Test 169: query (line 945)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_with_default'
ORDER BY ordinal_position;

-- Test 170: query (line 956)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_with_default'
ORDER BY ordinal_position;

-- Test 171: statement (line 966)
DROP TABLE IF EXISTS t_with_default;

-- Test 172: statement (line 971)
CREATE TABLE T_FOR_FUNCTION (C1 INT, C2 INT);

-- Test 173: statement (line 974)
CREATE OR REPLACE FUNCTION F1() RETURNS INT AS 'SELECT C2 FROM T_FOR_FUNCTION' LANGUAGE SQL;

-- Test 174: statement (line 977)
ALTER TABLE T_FOR_FUNCTION ALTER COLUMN C2 SET DATA TYPE TEXT;

-- Test 175: statement (line 980)
DROP FUNCTION F1;

-- Test 176: statement (line 983)
DROP TABLE IF EXISTS T_FOR_FUNCTION;

-- Test 177: statement (line 988)
CREATE TABLE t_ttl_w_expire_at (c1 int, expire_at TIMESTAMPTZ);

-- Test 178: statement (line 991)
ALTER TABLE t_ttl_w_expire_at ALTER COLUMN expire_at set data type timestamptz;

-- Test 179: statement (line 994)
ALTER TABLE t_ttl_w_expire_at ADD COLUMN new_expire_at TIMESTAMPTZ;

-- Test 180: statement (line 997)
-- ALTER TABLE t_ttl_w_expire_at SET (ttl_expiration_expression = new_expire_at);

-- Test 181: statement (line 1000)
ALTER TABLE t_ttl_w_expire_at ALTER COLUMN expire_at set data type timestamptz;

-- Test 182: statement (line 1003)
CREATE TABLE T_TTL_W_DEFAULT (C1 INT PRIMARY KEY, crdb_internal_expiration TIMESTAMPTZ);

-- Test 183: statement (line 1006)
ALTER TABLE T_TTL_W_DEFAULT ALTER COLUMN crdb_internal_expiration SET DATA TYPE TIMESTAMPTZ;

-- Test 184: statement (line 1009)
DROP TABLE IF EXISTS t_ttl_w_expire_at;
DROP TABLE IF EXISTS T_TTL_W_DEFAULT;

-- Test 185: statement (line 1015)
CREATE TABLE T1_FOR_SEQ (C1 INT);

-- Test 186: statement (line 1018)
CREATE SEQUENCE SEQ1 OWNED BY T1_FOR_SEQ.C1;

-- Test 187: statement (line 1021)
ALTER TABLE T1_FOR_SEQ ALTER COLUMN C1 SET DATA TYPE TEXT;

-- Test 188: statement (line 1025)
ALTER TABLE T1_FOR_SEQ ALTER COLUMN C1 SET DATA TYPE SMALLINT USING C1::SMALLINT;

-- Test 189: statement (line 1028)
DROP TABLE IF EXISTS T1_FOR_SEQ CASCADE;

-- Test 190: statement (line 1032)
DROP SEQUENCE IF EXISTS SEQ1;

-- Test 191: statement (line 1037)
CREATE TABLE t_bytes (c1 BYTEA, c2 BYTEA, c3 BYTEA);

-- Test 192: statement (line 1040)
INSERT INTO t_bytes VALUES ('hello', 'world', 'worldhello'),(NULL,NULL,NULL);

-- Test 193: query (line 1043)
SELECT * FROM t_bytes ORDER BY c1;

-- Test 194: query (line 1055)
SELECT c1 FROM t_bytes ORDER BY c1;

-- Test 195: statement (line 1064)
ALTER TABLE t_bytes ALTER COLUMN c2 SET DATA TYPE CHAR(4)
USING left(convert_from(c2, 'UTF8'), 4);

-- Test 196: query (line 1067)
SELECT c2, c3 FROM t_bytes ORDER BY c1;

-- Test 197: statement (line 1073)
UPDATE t_bytes SET c2 = 'w';

-- Test 198: statement (line 1076)
ALTER TABLE t_bytes ALTER COLUMN c2 SET DATA TYPE CHAR(4)
USING left(c2, 4);

-- Test 198: statement (line 1076)
UPDATE t_bytes
SET c3 = decode('00000000000000000000000000000000', 'hex')
WHERE c3 IS NOT NULL;

-- Test 199: statement (line 1079)
ALTER TABLE t_bytes ALTER COLUMN c3 SET DATA TYPE UUID
USING encode(c3, 'hex')::uuid;

-- Test 200: statement (line 1082)
ALTER TABLE t_bytes ALTER COLUMN c3 SET DATA TYPE UUID;

UPDATE t_bytes SET c3='3b5692c8-0f73-49ec-9186-8f1478f3064a' WHERE c1 IS NOT NULL;

ALTER TABLE t_bytes ALTER COLUMN c3 SET DATA TYPE UUID;

-- Test 203: query (line 1094)
SELECT c2,c3 FROM t_bytes ORDER BY c1;

-- Test 204: query (line 1100)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_bytes'
ORDER BY ordinal_position;

-- Test 205: query (line 1109)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_bytes'
ORDER BY ordinal_position;

-- Test 206: query (line 1123)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_bytes'
ORDER BY ordinal_position;

-- Test 207: statement (line 1136)
DROP TABLE IF EXISTS t_bytes;

-- Test 208: statement (line 1141)
CREATE TABLE t_decimal (c1 DECIMAL(20,5), c2 DECIMAL(10,5), c3 DECIMAL(10,0));

-- Test 209: statement (line 1144)
INSERT INTO t_decimal VALUES (12345.6, 1.23456, 44.1234),(NULL,NULL,NULL),(100012.34,4563.21,22.9871),(99.77777,99.77777,-5.51);

-- Test 210: query (line 1147)
SELECT * FROM t_decimal ORDER BY c1;

-- Test 211: statement (line 1156)
UPDATE t_decimal SET c1 = 10012.34 WHERE c1 = 100012.34;

-- Test 211: statement (line 1156)
ALTER TABLE t_decimal ALTER COLUMN c1 SET DATA TYPE DECIMAL(7,2);

-- Test 212: statement (line 1160)
UPDATE t_decimal SET c1 = 10012.34 WHERE c1 = 100012.34;

-- Test 213: statement (line 1163)
ALTER TABLE t_decimal ALTER COLUMN c1 SET DATA TYPE DECIMAL(7,2);

-- Test 214: statement (line 1166)
ALTER TABLE t_decimal ALTER COLUMN c2 SET DATA TYPE DECIMAL(10,2);

-- Test 215: statement (line 1169)
ALTER TABLE t_decimal ALTER COLUMN c3 SET DATA TYPE DECIMAL(5,2);

-- Test 216: statement (line 1172)
ALTER TABLE t_decimal ALTER COLUMN c3 SET DATA TYPE DECIMAL(6,1);

-- Test 217: query (line 1175)
SELECT * FROM t_decimal ORDER BY c1;

-- Test 218: query (line 1183)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_decimal'
ORDER BY ordinal_position;

-- Test 219: query (line 1192)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_decimal'
ORDER BY ordinal_position;

-- Test 220: query (line 1206)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_decimal'
ORDER BY ordinal_position;

-- Test 221: statement (line 1219)
DROP TABLE IF EXISTS t_decimal;

-- Test 222: statement (line 1227)
CREATE TABLE t_bit_string (
    pk INT PRIMARY KEY,
    c1 BIT(8),
    c2 VARBIT(8),
    c3 TEXT,
    c4 TEXT,
    c5 TEXT
);

-- Test 222: statement (line 1227)
INSERT INTO t_bit_string VALUES (1,B'10101010', B'10101010', 'hello', 'world', 'worldhello'),(2,NULL,NULL,NULL,NULL,NULL);

-- Test 223: query (line 1230)
SELECT c1,c2,c3,c4,c5 FROM t_bit_string ORDER BY pk;

-- Test 224: statement (line 1237)
ALTER TABLE t_bit_string ALTER COLUMN c1 SET DATA TYPE BIT(4)
USING substring(c1 from 1 for 4);

-- Test 224: statement (line 1237)
ALTER TABLE t_bit_string ALTER COLUMN c2 SET DATA TYPE VARBIT(4)
USING substring(c2 from 1 for 4);

-- Test 225: statement (line 1240)
UPDATE t_bit_string SET C2=B'1010' WHERE pk = 1;

-- Test 226: statement (line 1243)
ALTER TABLE t_bit_string ALTER COLUMN c1 SET DATA TYPE VARBIT(8), ALTER COLUMN c2 SET DATA TYPE VARBIT(4);

-- Test 227: statement (line 1247)
ALTER TABLE t_bit_string ALTER COLUMN c1 SET DATA TYPE VARBIT(8);

-- Test 228: statement (line 1250)
ALTER TABLE t_bit_string ALTER COLUMN c2 SET DATA TYPE VARBIT(4);

-- Test 229: statement (line 1254)
UPDATE t_bit_string SET C1=B'1010' WHERE pk = 1;

-- Test 230: statement (line 1257)
ALTER TABLE t_bit_string ALTER COLUMN c1 SET DATA TYPE BIT(4);

-- Test 231: query (line 1260)
SELECT c1,c2,c3,c4,c5 FROM t_bit_string ORDER BY pk;

-- Test 232: statement (line 1266)
ALTER TABLE t_bit_string ALTER COLUMN c3 SET DATA TYPE BYTEA USING C3::BYTEA;

-- Test 233: statement (line 1269)
ALTER TABLE t_bit_string ALTER COLUMN c3 SET DATA TYPE BYTEA;

-- Test 234: statement (line 1274)
ALTER TABLE t_bit_string ALTER COLUMN c4 SET DATA TYPE CHAR(4)
USING left(c4, 4);

-- Test 235: query (line 1277)
SELECT c1,c2,c3,c4,c5 FROM t_bit_string ORDER BY pk;

-- Test 236: statement (line 1283)
UPDATE t_bit_string SET c4 = 'worl' WHERE pk = 1;

-- Test 237: statement (line 1286)
ALTER TABLE t_bit_string ALTER COLUMN c4 SET DATA TYPE VARCHAR(4);

-- Test 238: query (line 1289)
SELECT c1,c2,c3,c4,c5 FROM t_bit_string ORDER BY pk;

-- Test 239: statement (line 1297)
ALTER TABLE t_bit_string ALTER COLUMN c5 SET DATA TYPE CHAR(6)
USING left(c5, 6);

-- Test 240: query (line 1300)
SELECT c1,c2,c3,c4,c5 FROM t_bit_string ORDER BY pk;

-- Test 241: statement (line 1306)
UPDATE t_bit_string SET c5 = 'worldh' WHERE pk = 1;

-- Test 242: statement (line 1309)
ALTER TABLE t_bit_string ALTER COLUMN c5 SET DATA TYPE CHAR(6);

-- Test 243: query (line 1312)
SELECT c1,c2,c3,c4,c5 FROM t_bit_string ORDER BY pk;

-- Test 244: query (line 1318)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_bit_string'
ORDER BY ordinal_position;

-- Test 245: statement (line 1328)
DROP TABLE IF EXISTS t_bit_string;

-- Test 246: statement (line 1333)
CREATE TABLE t_int (pk INT PRIMARY KEY, c1 BIGINT);

-- Test 247: statement (line 1336)
INSERT INTO t_int VALUES (1, 2147483648),(2,NULL);

-- Test 248: statement (line 1339)
UPDATE t_int SET c1 = 2147483647 WHERE pk = 1;

-- Test 248: statement (line 1339)
ALTER TABLE t_int ALTER COLUMN c1 SET DATA TYPE INTEGER;

-- Test 249: statement (line 1342)
UPDATE t_int SET c1 = c1 - 1 WHERE pk = 1;

-- Test 250: query (line 1345)
SELECT c1 FROM t_int ORDER BY pk;

-- Test 251: statement (line 1351)
ALTER TABLE t_int ALTER COLUMN c1 SET DATA TYPE INTEGER;

-- Test 252: statement (line 1354)
UPDATE t_int SET c1 = 32767 WHERE pk = 1;

-- Test 252: statement (line 1354)
ALTER TABLE t_int ALTER COLUMN c1 SET DATA TYPE SMALLINT;

-- Test 253: statement (line 1357)
UPDATE t_int SET c1 = 32767 WHERE pk = 1;

-- Test 254: statement (line 1360)
ALTER TABLE t_int ALTER COLUMN c1 SET DATA TYPE SMALLINT;

-- Test 255: query (line 1363)
SELECT c1 FROM t_int ORDER BY pk;

-- Test 256: query (line 1369)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_int'
ORDER BY ordinal_position;

-- Test 257: statement (line 1375)
DROP TABLE IF EXISTS t_int;

-- Test 258: statement (line 1380)
create table roach_village ( roach_id int primary key, name text, age smallint, legs smallint );

-- Test 259: statement (line 1383)
create index idx on roach_village(age) where name = 'papa roach';

-- Test 260: statement (line 1386)
alter table roach_village alter column name set data type char(20);

-- Test 261: statement (line 1389)
alter table roach_village alter column age set data type bigint;

-- Test 262: statement (line 1392)
alter table roach_village alter column legs set data type bigint;

-- Test 263: statement (line 1397)
create table t_many (c1 smallint);

-- Test 264: statement (line 1400)
insert into t_many values (0),(100),(-32);

-- Test 265: statement (line 1404)
alter table t_many alter column c1 set data type text;

-- Test 266: query (line 1407)
SELECT * FROM t_many;

-- Test 267: query (line 1414)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_many'
ORDER BY ordinal_position;

-- Test 268: statement (line 1421)
alter table t_many alter column c1 set data type smallint using c1::smallint;

-- Test 269: query (line 1424)
SELECT * FROM t_many;

-- Test 270: query (line 1431)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_many'
ORDER BY ordinal_position;

-- Test 271: statement (line 1437)
alter table t_many alter column c1 set not null, alter column c1 set data type VARCHAR(10) using concat(c1::text, 'boo');

-- Test 272: statement (line 1440)
alter table t_many alter column c1 set data type VARCHAR(10) using concat(c1::text, 'boo');

-- Test 273: query (line 1443)
SELECT c1 FROM t_many;

-- Test 274: query (line 1450)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_many'
ORDER BY ordinal_position;

-- Test 275: statement (line 1457)
alter table t_many alter column c1 set data type INTEGER using replace(c1, 'boo', '')::INTEGER;

-- Test 276: query (line 1460)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_many'
ORDER BY ordinal_position;

-- Test 277: statement (line 1467)
alter table t_many alter column c1 set data type INTEGER;

-- Test 278: query (line 1470)
SELECT * FROM t_many;

-- Test 279: query (line 1477)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_many'
ORDER BY ordinal_position;

-- Test 280: statement (line 1483)
DROP TABLE IF EXISTS t_many;

-- Test 281: statement (line 1488)
CREATE TABLE stored1 (A INT NOT NULL PRIMARY KEY, COMP1 SMALLINT);

-- Test 282: statement (line 1491)
INSERT INTO stored1 (A) VALUES (10),(150),(190),(2000);

-- Test 283: query (line 1494)
SELECT * FROM stored1 ORDER BY A;

-- Test 284: statement (line 1503)
ALTER TABLE stored1 ALTER COLUMN COMP1 SET DATA TYPE BIGINT;

-- onlyif config schema-locked-disabled

-- Test 285: query (line 1507)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'stored1'
ORDER BY ordinal_position;

-- Test 286: query (line 1518)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'stored1'
ORDER BY ordinal_position;

-- Test 287: query (line 1528)
SELECT * FROM stored1 ORDER BY A;

-- Test 288: statement (line 1537)
INSERT INTO stored1 (A) VALUES (2147483647),(2147483646);

-- Test 289: statement (line 1540)
ALTER TABLE stored1 ALTER COLUMN COMP1 SET DATA TYPE INTEGER;

-- Test 290: statement (line 1543)
DELETE FROM stored1 WHERE a = 2147483648;

-- Test 291: statement (line 1546)
ALTER TABLE stored1 ALTER COLUMN COMP1 SET DATA TYPE INTEGER;

-- onlyif config schema-locked-disabled

-- Test 292: query (line 1550)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'stored1'
ORDER BY ordinal_position;

-- Test 293: query (line 1561)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'stored1'
ORDER BY ordinal_position;

-- Test 294: query (line 1571)
SELECT * FROM stored1 ORDER BY A;

-- Test 295: statement (line 1581)
ALTER TABLE stored1 ALTER COLUMN comp1 SET DATA TYPE BOOLEAN USING (comp1 <> 0);

-- Test 296: statement (line 1584)
ALTER TABLE stored1 ALTER COLUMN comp1 SET DATA TYPE TEXT;

-- Test 297: statement (line 1589)
ALTER TABLE stored1 ALTER COLUMN comp1 SET DATA TYPE SMALLINT USING -1;

-- Test 298: statement (line 1592)
INSERT INTO stored1 (A) VALUES (-1000);

-- onlyif config schema-locked-disabled

-- Test 299: query (line 1596)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'stored1'
ORDER BY ordinal_position;

-- Test 300: query (line 1607)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'stored1'
ORDER BY ordinal_position;

-- Test 301: query (line 1617)
SELECT * FROM stored1 ORDER BY A;

-- Test 302: statement (line 1628)
-- ALTER TABLE stored1 SET (schema_locked=false);

-- Test 303: statement (line 1632)
ALTER TABLE stored1 ALTER COLUMN comp1 SET DATA TYPE INTEGER USING -1;

-- skipif config schema-locked-disabled

-- Test 304: statement (line 1636)
-- ALTER TABLE stored1 SET (schema_locked=true);

-- Test 305: statement (line 1639)
DROP TABLE IF EXISTS stored1;

-- Test 306: statement (line 1644)
CREATE TABLE virt1 (c1 BIGINT NOT NULL PRIMARY KEY, v1 BIGINT);

-- Test 307: statement (line 1647)
INSERT INTO virt1 (c1) VALUES (100), (2147483647);

-- Test 308: statement (line 1651)
ALTER TABLE virt1 ALTER COLUMN v1 SET DATA TYPE INTEGER USING 10;

-- Test 309: statement (line 1654)
ALTER TABLE virt1 ALTER COLUMN v1 SET DATA TYPE INTEGER;

-- Test 310: query (line 1657)
SELECT * from virt1 ORDER BY c1;

-- Test 311: query (line 1664)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt1'
ORDER BY ordinal_position;

-- Test 312: query (line 1674)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt1'
ORDER BY ordinal_position;

-- Test 313: statement (line 1683)
ALTER TABLE virt1 ALTER COLUMN v1 SET DATA TYPE SMALLINT;

-- Test 314: statement (line 1686)
DELETE FROM virt1 WHERE c1 = 2147483647;

-- Test 315: statement (line 1689)
ALTER TABLE virt1 ALTER COLUMN v1 SET DATA TYPE SMALLINT;

-- Test 316: statement (line 1692)
INSERT INTO virt1 (c1) VALUES (-9999);

-- Test 317: query (line 1695)
SELECT * from virt1 ORDER BY c1;

-- Test 318: query (line 1702)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt1'
ORDER BY ordinal_position;

-- Test 319: query (line 1712)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt1'
ORDER BY ordinal_position;

-- Test 320: statement (line 1721)
ALTER TABLE virt1 ALTER COLUMN v1 SET DATA TYPE TEXT;

-- Test 321: statement (line 1724)
ALTER TABLE virt1 ALTER COLUMN v1 SET DATA TYPE FLOAT USING v1::double precision;

-- Test 322: statement (line 1727)
DROP TABLE IF EXISTS virt1;

-- Test 323: statement (line 1732)
CREATE TABLE virt2 (C1 TIMESTAMP(6) NOT NULL PRIMARY KEY, v1 TIMESTAMP(3));

-- Test 324: statement (line 1735)
INSERT INTO virt2 (c1) VALUES ('2024-10-31 16:50:00.123456');

-- Test 325: query (line 1738)
SELECT * FROM virt2 ORDER BY c1;

-- Test 326: statement (line 1743)
ALTER TABLE virt2 ALTER COLUMN v1 SET DATA TYPE TIMESTAMP(2);

-- Test 327: query (line 1746)
SELECT * FROM virt2 ORDER BY c1;

-- Test 328: query (line 1752)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt2'
ORDER BY ordinal_position;

-- Test 329: query (line 1762)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt2'
ORDER BY ordinal_position;

-- Test 330: statement (line 1771)
ALTER TABLE virt2 ALTER COLUMN v1 SET DATA TYPE TIMESTAMP(5);

-- Test 331: query (line 1774)
SELECT * FROM virt2 ORDER BY c1;

-- Test 332: query (line 1780)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt2'
ORDER BY ordinal_position;

-- Test 333: query (line 1790)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt2'
ORDER BY ordinal_position;

-- Test 334: statement (line 1799)
DROP TABLE IF EXISTS virt2;

-- Test 335: statement (line 1804)
CREATE TABLE virt3 (C1 DECIMAL(6,3) NOT NULL PRIMARY KEY, v1 DECIMAL(6,3));

-- Test 336: statement (line 1807)
INSERT INTO virt3 (c1) VALUES (1.23456),(12.34567),(-123.4);

-- Test 337: query (line 1810)
SELECT * FROM virt3 ORDER BY c1;

-- Test 338: statement (line 1817)
ALTER TABLE virt3 ALTER COLUMN v1 SET DATA TYPE DECIMAL(6,2);

-- Test 339: query (line 1820)
SELECT * FROM virt3 ORDER BY c1;

-- Test 340: query (line 1828)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt3'
ORDER BY ordinal_position;

-- Test 341: query (line 1838)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt3'
ORDER BY ordinal_position;

-- Test 342: statement (line 1847)
ALTER TABLE virt3 ALTER COLUMN v1 SET DATA TYPE DECIMAL(5,4);

-- Test 343: statement (line 1850)
ALTER TABLE virt3 ALTER COLUMN v1 SET DATA TYPE DECIMAL(8,4);

-- Test 344: query (line 1853)
SELECT * FROM virt3 ORDER BY c1;

-- Test 345: query (line 1861)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt3'
ORDER BY ordinal_position;

-- Test 346: query (line 1871)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'virt3'
ORDER BY ordinal_position;

-- Test 347: statement (line 1880)
DROP TABLE IF EXISTS virt3;

-- Test 348: statement (line 1889)
-- set use_declarative_schema_changer = 'unsafe_always';

-- Test 349: statement (line 1892)
create table t_droppedcol (dropme int);

-- Test 350: statement (line 1895)
-- SET autocommit_before_ddl = false

-- Test 351: statement (line 1898)
begin;

-- Test 352: statement (line 1901)
alter table t_droppedcol drop column dropme;

-- Test 353: statement (line 1904)
alter table t_droppedcol add column dropme text;

-- Test 354: statement (line 1907)
rollback;

-- Test 355: statement (line 1910)
-- RESET autocommit_before_ddl;

-- onlyif config schema-locked-disabled

-- Test 356: query (line 1914)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_droppedcol'
ORDER BY ordinal_position;

-- Test 357: statement (line 1923)
DROP TABLE IF EXISTS t_droppedcol;

-- Test 358: statement (line 1927)
-- SET use_declarative_schema_changer = $schema_changer_state

-- Test 359: statement (line 1936)
-- set use_declarative_schema_changer = 'unsafe_always';

-- Test 360: statement (line 1939)
CREATE TABLE t_multi_alter (c1 INT NOT NULL PRIMARY KEY, c2 TEXT NOT NULL, drop DATE);

-- Test 361: statement (line 1942)
INSERT INTO t_multi_alter VALUES (10,'ten','1984-09-06'),(20,'twenty','2024-08-10');

-- Test 362: statement (line 1948)
begin;
ALTER TABLE t_multi_alter ADD COLUMN c3 TEXT DEFAULT '<default>', DROP COLUMN drop;
ALTER TABLE t_multi_alter ALTER COLUMN c2 SET DATA TYPE BIGINT USING c1::BIGINT;
commit;

-- Test 363: query (line 1954)
select * from t_multi_alter order by c1;

-- Test 364: statement (line 1960)
begin;
ALTER TABLE t_multi_alter ALTER COLUMN c3 SET DEFAULT NULL;
ALTER TABLE t_multi_alter ALTER COLUMN c3 SET DATA TYPE DATE USING '2024-04-22'::DATE;
commit;

-- Test 365: query (line 1966)
select * from t_multi_alter order by c1;

-- Test 366: query (line 1973)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_multi_alter'
ORDER BY ordinal_position;

-- Test 367: query (line 1986)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't_multi_alter'
ORDER BY ordinal_position;

-- Test 368: statement (line 1998)
DROP TABLE IF EXISTS t_multi_alter;

-- Test 369: statement (line 2002)
-- SET use_declarative_schema_changer = $schema_changer_state

RESET client_min_messages;
