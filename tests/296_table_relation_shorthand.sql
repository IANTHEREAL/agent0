-- Issue #1563: PostgreSQL TABLE <relation> shorthand should parse and execute.

DROP TABLE IF EXISTS table_relation_shorthand_296;

CREATE TABLE table_relation_shorthand_296 (
    id INT PRIMARY KEY,
    name TEXT
);

INSERT INTO table_relation_shorthand_296 VALUES (1, 'alpha');

TABLE table_relation_shorthand_296;
TABLE public.table_relation_shorthand_296;
TABLE "table_relation_shorthand_296";
TABLE ONLY table_relation_shorthand_296;
TABLE table_relation_shorthand_296 *;
TABLE table_relation_shorthand_296 ORDER BY id;
TABLE table_relation_shorthand_296 LIMIT 5;
TABLE table_relation_shorthand_296 OFFSET 2;
TABLE table_relation_shorthand_296 FETCH FIRST 1 ROW ONLY;
TABLE table_relation_shorthand_296 FOR UPDATE;
INSERT INTO table_relation_shorthand_296 VALUES (2, 'beta');
TABLE ONLY table_relation_shorthand_296 ORDER BY id DESC LIMIT 1;
TABLE table_relation_shorthand_296 * ORDER BY id DESC LIMIT 1;

-- Negative control: unresolved relation should still return RelationNotFound.
TABLE table_relation_shorthand_missing_296;

DROP TABLE table_relation_shorthand_296;
