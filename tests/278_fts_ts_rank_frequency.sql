-- ts_rank term frequency tests (#1219)
-- Verifies that ts_rank returns higher scores for documents with more
-- occurrences of the matched term.

-- Term frequency: 3x occurrence must score higher than 1x (simple config)
SELECT
    ts_rank(to_tsvector('simple', 'database database database'),
            plainto_tsquery('simple', 'database')) >
    ts_rank(to_tsvector('simple', 'database'),
            plainto_tsquery('simple', 'database')) AS freq_matters_simple;

-- Term frequency: 3x vs 1x (english config)
SELECT
    ts_rank(to_tsvector('english', 'database database database'),
            plainto_tsquery('english', 'database')) >
    ts_rank(to_tsvector('english', 'database'),
            plainto_tsquery('english', 'database')) AS freq_matters_english;

-- Normalization flag 2 (LENGTH) reduces score
SELECT
    ts_rank(to_tsvector('simple', 'hello world'), plainto_tsquery('simple', 'hello'), 2) <
    ts_rank(to_tsvector('simple', 'hello world'), plainto_tsquery('simple', 'hello'), 0) AS norm_2_reduces;

-- Normalization flag 1 (LOGLENGTH) reduces score
SELECT
    ts_rank(to_tsvector('simple', 'hello world'), plainto_tsquery('simple', 'hello'), 1) <
    ts_rank(to_tsvector('simple', 'hello world'), plainto_tsquery('simple', 'hello'), 0) AS norm_1_reduces;

-- Normalization flag 32 (RDIVRPLUS1) reduces score
SELECT
    ts_rank(to_tsvector('simple', 'hello world'), plainto_tsquery('simple', 'hello'), 32) <
    ts_rank(to_tsvector('simple', 'hello world'), plainto_tsquery('simple', 'hello'), 0) AS norm_32_reduces;

-- ts_rank_cd also respects term frequency
SELECT
    ts_rank_cd(to_tsvector('simple', 'hello hello hello'),
               plainto_tsquery('simple', 'hello')) >
    ts_rank_cd(to_tsvector('simple', 'hello'),
               plainto_tsquery('simple', 'hello')) AS cd_freq_matters;

-- Weight A produces higher score than weight D (default)
SELECT
    ts_rank(setweight(to_tsvector('simple', 'hello'), 'A'),
            plainto_tsquery('simple', 'hello')) >
    ts_rank(to_tsvector('simple', 'hello'),
            plainto_tsquery('simple', 'hello')) AS weight_a_higher;

-- Verify tsvector positions are preserved for repeated words
SELECT to_tsvector('simple', 'database database database');
