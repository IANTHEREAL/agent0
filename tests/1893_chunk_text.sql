-- CHUNK_TEXT: Smart document chunking TVF for vector embeddings
-- Issue: #1893, #1892

-- 1. Basic: short document returns single chunk
SELECT chunk_index, chunk_text, chunk_pos FROM CHUNK_TEXT('Hello, world!');

-- 2. Empty string returns no rows
SELECT COUNT(*) AS cnt FROM CHUNK_TEXT('');

-- 3. NULL input returns no rows
SELECT COUNT(*) AS cnt FROM CHUNK_TEXT(NULL);

-- 4. Custom max_chars: force multiple chunks from moderate text
SELECT chunk_index, length(chunk_text) AS len, chunk_pos
FROM CHUNK_TEXT(repeat('abcdefgh ', 500), 200, 30);

-- 5. Chunks are sequential
SELECT chunk_index FROM CHUNK_TEXT(repeat('word ', 1000), 100, 15) ORDER BY chunk_index;

-- 6. With title parameter: output includes title prefix
SELECT chunk_index, chunk_text FROM CHUNK_TEXT('Short doc content', 3600, 540, 'My Document');

-- 7. Title parameter NULL: no prefix
SELECT chunk_index, chunk_text FROM CHUNK_TEXT('Plain content');

-- 8. Markdown heading respected as break point
SELECT chunk_index, left(chunk_text, 40) AS preview
FROM CHUNK_TEXT(
    concat(repeat('x', 3200), E'\n## Section Two\n', repeat('y', 2000)),
    3600, 540
);

-- 9. Code fence not split
SELECT chunk_index, chunk_text LIKE '%```%partial%' AS has_partial_fence
FROM CHUNK_TEXT(
    concat('intro text\n```python\n', repeat('code_line\n', 50), '```\nafter code\n', repeat('z', 4000)),
    300, 45
);

-- 10. Named parameters
SELECT chunk_index, chunk_pos
FROM CHUNK_TEXT('Hello named params test content here', max_chars => 20, overlap_chars => 5);

-- 11. Use in subquery / CTE
WITH chunks AS (
    SELECT chunk_index, chunk_text, chunk_pos
    FROM CHUNK_TEXT('First paragraph.\n\nSecond paragraph.\n\nThird paragraph.', 30, 5)
)
SELECT COUNT(*) AS total_chunks FROM chunks;
