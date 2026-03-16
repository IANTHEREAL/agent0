-- CHUNK_TEXT: Smart document chunking TVF for vector embeddings
-- Issue: #1893, #1892

-- 1. Basic: short document returns single chunk
SELECT 'basic_single' AS test_name, chunk_index, chunk_text, chunk_pos
FROM CHUNK_TEXT('Hello, world!');

-- 2. Empty string returns no rows
SELECT 'empty_count' AS test_name, COUNT(*) AS cnt FROM CHUNK_TEXT('');

-- 3. NULL input returns no rows
SELECT 'null_count' AS test_name, COUNT(*) AS cnt FROM CHUNK_TEXT(NULL);

-- 4. With title: output includes title prefix
SELECT 'title_prefix' AS test_name, chunk_text
FROM CHUNK_TEXT('Short doc content', 3600, 540, 'My Document');

-- 5. Without title: no prefix
SELECT 'no_prefix' AS test_name, chunk_text
FROM CHUNK_TEXT('Plain content');

-- 6. Multiple chunks with custom params
SELECT 'multi_chunk' AS test_name, COUNT(*) AS cnt
FROM CHUNK_TEXT(repeat('abcdefgh ', 500), 200, 30);

-- 7. CTE usage
WITH chunks AS (
    SELECT chunk_index, chunk_text, chunk_pos
    FROM CHUNK_TEXT(repeat('hello world ', 400), 200, 30)
)
SELECT 'cte_works' AS test_name, COUNT(*) > 0 AS has_chunks FROM chunks;

-- 8. Named parameters
SELECT 'named_params' AS test_name, COUNT(*) > 0 AS has_chunks
FROM CHUNK_TEXT('test content for named params usage', max_chars => 20, overlap_chars => 5);
