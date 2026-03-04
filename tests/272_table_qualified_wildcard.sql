-- Regression test for issue #1376: table-qualified wildcard SELECT (table.*)
-- ORM-generated queries commonly emit SELECT "table".* FROM "table".

DROP TABLE IF EXISTS tqw_items;

CREATE TABLE tqw_items(id INT PRIMARY KEY, name TEXT, qty INT);
INSERT INTO tqw_items VALUES (1, 'apple', 10), (2, 'banana', 20);

-- Unquoted table-qualified wildcard
SELECT tqw_items.* FROM tqw_items ORDER BY id;

-- Quoted table-qualified wildcard (the ORM pattern that was broken)
SELECT "tqw_items".* FROM "tqw_items" ORDER BY id;

-- With alias
SELECT t.* FROM tqw_items t ORDER BY id;

-- Quoted alias
SELECT "t".* FROM tqw_items "t" ORDER BY id;

DROP TABLE tqw_items;
