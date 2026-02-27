-- FTS Phase 3: Phrase operators, phraseto_tsquery, websearch_to_tsquery, ts_headline

-- Create isolated test database
CREATE DATABASE fts_phase3_test;
\c fts_phase3_test

-- Section 1: Phrase operators with to_tsquery
SELECT to_tsquery('simple', '''hello'' <-> ''world''');
SELECT to_tsquery('simple', '''hello'' <2> ''world''');

-- Section 2: Phrase matching with @@
SELECT to_tsvector('simple', 'hello world') @@ to_tsquery('simple', '''hello'' <-> ''world''');
SELECT to_tsvector('simple', 'hello big world') @@ to_tsquery('simple', '''hello'' <-> ''world''');
SELECT to_tsvector('simple', 'hello big world') @@ to_tsquery('simple', '''hello'' <2> ''world''');

-- Section 3: phraseto_tsquery
SELECT phraseto_tsquery('simple', 'hello world');
SELECT phraseto_tsquery('simple', 'quick brown fox');
SELECT phraseto_tsquery('english', 'the fat cat');
SELECT phraseto_tsquery('english', 'the cat is big');
SELECT phraseto_tsquery('simple', 'cat');
SELECT phraseto_tsquery('english', 'the');

-- Section 4: phraseto_tsquery with @@ matching
SELECT to_tsvector('simple', 'hello world test') @@ phraseto_tsquery('simple', 'hello world');
SELECT to_tsvector('simple', 'hello test world') @@ phraseto_tsquery('simple', 'hello world');

-- Section 5: websearch_to_tsquery
SELECT websearch_to_tsquery('simple', 'hello world');
SELECT websearch_to_tsquery('simple', '"hello world"');
SELECT websearch_to_tsquery('simple', 'hello -world');
SELECT websearch_to_tsquery('simple', 'hello or world');
SELECT websearch_to_tsquery('simple', 'hello OR world');
SELECT websearch_to_tsquery('simple', '');
SELECT websearch_to_tsquery('simple', 'quick "brown fox" -lazy');

-- Section 6: websearch_to_tsquery never errors on garbage
SELECT websearch_to_tsquery('simple', '!@#$%^&*()');

-- Section 7: ts_headline basic
SELECT ts_headline('simple', 'the quick brown fox jumps', to_tsquery('simple', 'fox'));
SELECT ts_headline('simple', 'hello world', to_tsquery('simple', 'hello'), 'StartSel=<em>, StopSel=</em>');

-- Section 8: ts_headline with no match returns unchanged
SELECT ts_headline('simple', 'hello world', to_tsquery('simple', 'xyz'));

-- Section 9: ts_rank with phrase queries (should not crash on <-> syntax)
SELECT ts_rank(to_tsvector('simple', 'hello world'), to_tsquery('simple', '''hello'' <-> ''world''')) > 0;

-- Section 10: ts_rank_cd returns different from ts_rank
SELECT ts_rank_cd(to_tsvector('simple', 'hello world'), plainto_tsquery('simple', 'hello world')) > 0;

-- Section 11: Phrase + GIN index (GIN treats <-> as AND for candidate filtering)
CREATE TABLE fts_phrase_test (id SERIAL PRIMARY KEY, doc TEXT, tsv TSVECTOR);
INSERT INTO fts_phrase_test (doc, tsv) VALUES
    ('hello world test', to_tsvector('simple', 'hello world test')),
    ('hello big world', to_tsvector('simple', 'hello big world')),
    ('world hello', to_tsvector('simple', 'world hello'));
CREATE INDEX idx_fts_phrase_gin ON fts_phrase_test USING gin (tsv);

-- GIN should return candidates, then recheck filters for phrase semantics
SELECT id, doc FROM fts_phrase_test WHERE tsv @@ to_tsquery('simple', '''hello'' <-> ''world''') ORDER BY id;

-- GIN with phraseto_tsquery
SELECT id, doc FROM fts_phrase_test WHERE tsv @@ phraseto_tsquery('simple', 'hello world') ORDER BY id;

-- Section 12: websearch_to_tsquery with GIN
SELECT id, doc FROM fts_phrase_test WHERE tsv @@ websearch_to_tsquery('simple', '"hello world"') ORDER BY id;

-- Section 13: ts_headline with actual query
SELECT ts_headline('simple', doc, to_tsquery('simple', 'hello')) FROM fts_phrase_test WHERE id = 1;

-- Clean up
DROP TABLE fts_phrase_test;
\c postgres
DROP DATABASE fts_phase3_test;
