-- ============================================================================
-- RAG Ingest Pipeline via pg_cron + OpenAI Embeddings
-- 
-- Demonstrates: fs9 file reading → OpenAI embedding via http_post →
-- vector + FTS hybrid store, all orchestrated by pg_cron.
--
-- Pattern:
--   cron fires every minute
--   → list files in /inbox/
--   → skip already-processed (via tracking table)
--   → read content → call OpenAI embeddings API → store vector + tsvector
--   → mark as processed
--   → when /inbox/ is empty, cron is a no-op
--
-- Prerequisites:
--   - pg-tikv with fs9 + http extensions
--   - Files uploaded to /inbox/ via `db9 fs cp` or SDK client.fs.write()
--   - Set OPENAI_API_KEY below (or use a self-hosted model URL)
--
-- Usage:
--   1. Edit OPENAI_API_KEY in the cron job SQL below
--   2. Upload files:  db9 fs cp *.txt <db-id>:/inbox/
--   3. Run this script to set up tables + cron job
--   4. Monitor:  SELECT * FROM rag_ingest_status;
--   5. Search:   SELECT * FROM rag_search('your query here');
-- ============================================================================

-- ============================================================================
-- SETUP: Extensions
-- ============================================================================

CREATE EXTENSION IF NOT EXISTS http;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- ============================================================================
-- SETUP: Tables
-- ============================================================================

DROP TABLE IF EXISTS rag_processed CASCADE;
CREATE TABLE rag_processed (
    path         TEXT PRIMARY KEY,
    file_size    BIGINT,
    processed_at TIMESTAMP DEFAULT NOW()
);

DROP TABLE IF EXISTS rag_documents CASCADE;
CREATE TABLE rag_documents (
    id          BIGSERIAL PRIMARY KEY,
    file_path   TEXT NOT NULL,
    content     TEXT NOT NULL,
    file_size   BIGINT,
    tsv         TSVECTOR,
    embedding   VECTOR(1536),
    ingested_at TIMESTAMP DEFAULT NOW()
);

SET tipg.use_optimizer = off;
CREATE INDEX idx_rag_fts ON rag_documents USING GIN (tsv);
SET tipg.use_optimizer = on;

-- ============================================================================
-- HELPER: Search (hybrid FTS + vector)
-- ============================================================================

CREATE OR REPLACE FUNCTION rag_search(query TEXT)
RETURNS TABLE(file_path TEXT, snippet TEXT, rank REAL)
LANGUAGE sql AS $$
    SELECT
        d.file_path,
        LEFT(d.content, 200) AS snippet,
        ts_rank(d.tsv, plainto_tsquery('english', query)) AS rank
    FROM rag_documents d
    WHERE d.tsv @@ plainto_tsquery('english', query)
    ORDER BY rank DESC
    LIMIT 20;
$$;

DROP VIEW IF EXISTS rag_ingest_status;
CREATE VIEW rag_ingest_status AS
SELECT
    (SELECT count(*) FROM rag_documents)   AS documents_ingested,
    (SELECT count(*) FROM rag_processed)   AS files_processed,
    (SELECT max(processed_at) FROM rag_processed) AS last_ingest_at;

-- ============================================================================
-- CRON JOB: The "loop"
--
-- Runs every minute. Each run:
--   1. Lists unprocessed files in /inbox/
--   2. Reads file content via fs9_read
--   3. Calls OpenAI embeddings API via http_post with Authorization header
--   4. Inserts content + tsvector + vector into rag_documents
--   5. Marks files as processed
--
-- NOTE: Replace 'sk-YOUR-KEY-HERE' with your actual OpenAI API key.
--       For self-hosted models (Ollama, vLLM), change the URL and
--       remove/change the Authorization header.
--
-- LIMIT 1: processes one file per cron tick because each http_post
-- is a separate API call; pg-tikv limits 5 HTTP requests per statement.
-- ============================================================================

SELECT cron.unschedule('rag_ingest') WHERE EXISTS (
    SELECT 1 FROM cron.job WHERE jobname = 'rag_ingest'
);

SELECT cron.schedule('rag_ingest', '* * * * *', $$

    WITH new_file AS (
        SELECT f.path, f.size
        FROM extensions.fs9('/inbox/') f
        LEFT JOIN rag_processed p ON p.path = f.path
        WHERE f.type = 'file'
          AND p.path IS NULL
        ORDER BY f.path
        LIMIT 1
    ),
    file_content AS (
        SELECT
            path,
            size,
            fs9_read(path) AS content
        FROM new_file
    ),
    embedded AS (
        SELECT
            fc.path,
            fc.size,
            fc.content,
            (SELECT content FROM extensions.http_post(
                'https://api.openai.com/v1/embeddings',
                json_build_object(
                    'model', 'text-embedding-3-small',
                    'input', fc.content
                )::text,
                'application/json',
                '{"Authorization":"Bearer sk-YOUR-KEY-HERE"}'
            ))::jsonb -> 'data' -> 0 -> 'embedding' AS vec_json
        FROM file_content fc
    ),
    inserted AS (
        INSERT INTO rag_documents (file_path, content, file_size, tsv, embedding)
        SELECT
            path,
            content,
            size,
            to_tsvector('english', content),
            vec_json::text::vector
        FROM embedded
        RETURNING file_path, file_size
    )
    INSERT INTO rag_processed (path, file_size)
    SELECT file_path, file_size FROM inserted;

$$);

-- ============================================================================
-- VERIFY
-- ============================================================================

SELECT jobid, jobname, schedule, command
FROM cron.job
WHERE jobname = 'rag_ingest';

-- ============================================================================
-- SELF-HOSTED MODEL VARIANT (Ollama / vLLM)
--
-- Replace the embedded CTE above with:
--
--   embedded AS (
--       SELECT
--           fc.path, fc.size, fc.content,
--           (SELECT content FROM extensions.http_post(
--               'http://localhost:11434/api/embeddings',
--               json_build_object('model', 'all-minilm', 'prompt', fc.content)::text,
--               'application/json'
--           ))::jsonb -> 'embedding' AS vec_json
--       FROM file_content fc
--   )
--
-- No Authorization header needed for local models.
-- Change VECTOR(1536) to match your model's dimension (e.g., VECTOR(384)).
-- ============================================================================

-- ============================================================================
-- UNIVERSAL http() FUNCTION VARIANT
--
-- Instead of http_post with 4 args, you can use the universal http():
--
--   SELECT content FROM extensions.http(
--       'POST',
--       'https://api.openai.com/v1/embeddings',
--       '{"Authorization":"Bearer sk-YOUR-KEY-HERE"}',
--       'application/json',
--       json_build_object('model', 'text-embedding-3-small', 'input', 'hello')::text
--   );
--
-- Supported methods: GET, POST, PUT, DELETE, HEAD, PATCH
-- ============================================================================

-- ============================================================================
-- MONITORING
-- ============================================================================

-- Check ingest progress:
--   SELECT * FROM rag_ingest_status;
--
-- Check cron execution history:
--   SELECT runid, job_id, status, return_message, start_time, end_time
--   FROM cron.job_run_details
--   WHERE job_id = (SELECT jobid FROM cron.job WHERE jobname = 'rag_ingest')
--   ORDER BY runid DESC LIMIT 10;
--
-- Search documents (FTS):
--   SELECT * FROM rag_search('your search terms');
--
-- Search documents (vector similarity):
--   WITH q AS (
--       SELECT (SELECT content FROM extensions.http_post(
--           'https://api.openai.com/v1/embeddings',
--           json_build_object('model', 'text-embedding-3-small', 'input', 'your query')::text,
--           'application/json',
--           '{"Authorization":"Bearer sk-YOUR-KEY-HERE"}'
--       ))::jsonb -> 'data' -> 0 -> 'embedding' AS vec
--   )
--   SELECT d.file_path, LEFT(d.content, 200) AS snippet,
--          d.embedding <-> (q.vec::text::vector) AS distance
--   FROM rag_documents d, q
--   ORDER BY distance
--   LIMIT 10;
--
-- Stop the pipeline:
--   SELECT cron.unschedule('rag_ingest');

-- ============================================================================
-- SETUP: Extensions
-- ============================================================================

CREATE EXTENSION IF NOT EXISTS pg_cron;

-- ============================================================================
-- SETUP: Tables
-- ============================================================================

-- Tracking table: which files have been processed
DROP TABLE IF EXISTS rag_processed CASCADE;
CREATE TABLE rag_processed (
    path         TEXT PRIMARY KEY,
    file_size    BIGINT,
    processed_at TIMESTAMP DEFAULT NOW()
);

-- Document store with full-text search
DROP TABLE IF EXISTS rag_documents CASCADE;
CREATE TABLE rag_documents (
    id          BIGSERIAL PRIMARY KEY,
    file_path   TEXT NOT NULL,
    content     TEXT NOT NULL,
    file_size   BIGINT,
    tsv         TSVECTOR,
    ingested_at TIMESTAMP DEFAULT NOW()
);

-- GIN index for full-text search
SET tipg.use_optimizer = off;
CREATE INDEX idx_rag_fts ON rag_documents USING GIN (tsv);
SET tipg.use_optimizer = on;

-- ============================================================================
-- HELPER: Search function (convenience wrapper)
-- ============================================================================

-- Search documents by keyword
-- Usage: SELECT * FROM rag_search('database distributed');
CREATE OR REPLACE FUNCTION rag_search(query TEXT)
RETURNS TABLE(file_path TEXT, snippet TEXT, rank REAL)
LANGUAGE sql AS $$
    SELECT
        d.file_path,
        LEFT(d.content, 200) AS snippet,
        ts_rank(d.tsv, plainto_tsquery('english', query)) AS rank
    FROM rag_documents d
    WHERE d.tsv @@ plainto_tsquery('english', query)
    ORDER BY rank DESC
    LIMIT 20;
$$;

-- ============================================================================
-- HELPER: Status view
-- ============================================================================

DROP VIEW IF EXISTS rag_ingest_status;
CREATE VIEW rag_ingest_status AS
SELECT
    (SELECT count(*) FROM rag_documents)   AS documents_ingested,
    (SELECT count(*) FROM rag_processed)   AS files_processed,
    (SELECT max(processed_at) FROM rag_processed) AS last_ingest_at;

-- ============================================================================
-- CRON JOB: The "loop"
--
-- Runs every minute. Each run:
--   1. Lists files in /inbox/ that aren't in rag_processed
--   2. Reads up to 10 files
--   3. Inserts content + tsvector into rag_documents
--   4. Marks files as processed
--   5. If no new files → 0 rows affected → fast no-op
-- ============================================================================

SELECT cron.unschedule('rag_ingest') WHERE EXISTS (
    SELECT 1 FROM cron.job WHERE jobname = 'rag_ingest'
);

SELECT cron.schedule('rag_ingest', '* * * * *', $$

    WITH new_files AS (
        SELECT f.path, f.size
        FROM extensions.fs9('/inbox/') f
        LEFT JOIN rag_processed p ON p.path = f.path
        WHERE f.type = 'file'
          AND p.path IS NULL
        ORDER BY f.path
        LIMIT 10
    ),
    file_contents AS (
        SELECT
            path,
            size,
            fs9_read(path) AS content
        FROM new_files
    ),
    inserted AS (
        INSERT INTO rag_documents (file_path, content, file_size, tsv)
        SELECT
            path,
            content,
            size,
            to_tsvector('english', content)
        FROM file_contents
        RETURNING file_path, file_size
    )
    INSERT INTO rag_processed (path, file_size)
    SELECT file_path, file_size FROM inserted;

$$);

-- ============================================================================
-- VERIFY: Show scheduled job
-- ============================================================================

SELECT jobid, jobname, schedule, command
FROM cron.job
WHERE jobname = 'rag_ingest';

-- ============================================================================
-- MANUAL TEST: Run the pipeline once without waiting for cron
-- (Uncomment to test immediately after uploading files to /inbox/)
-- ============================================================================

-- WITH new_files AS (
--     SELECT f.path, f.size
--     FROM extensions.fs9('/inbox/') f
--     LEFT JOIN rag_processed p ON p.path = f.path
--     WHERE f.type = 'file'
--       AND p.path IS NULL
--     ORDER BY f.path
--     LIMIT 10
-- ),
-- file_contents AS (
--     SELECT path, size, fs9_read(path) AS content
--     FROM new_files
-- ),
-- inserted AS (
--     INSERT INTO rag_documents (file_path, content, file_size, tsv)
--     SELECT path, content, size, to_tsvector('english', content)
--     FROM file_contents
--     RETURNING file_path, file_size
-- )
-- INSERT INTO rag_processed (path, file_size)
-- SELECT file_path, file_size FROM inserted;

-- ============================================================================
-- MONITORING
-- ============================================================================

-- Check ingest progress:
--   SELECT * FROM rag_ingest_status;
--
-- Check cron execution history:
--   SELECT runid, job_id, status, return_message, start_time, end_time
--   FROM cron.job_run_details
--   WHERE job_id = (SELECT jobid FROM cron.job WHERE jobname = 'rag_ingest')
--   ORDER BY runid DESC LIMIT 10;
--
-- Search documents:
--   SELECT * FROM rag_search('your search terms');
--
-- Stop the pipeline:
--   SELECT cron.unschedule('rag_ingest');

-- ============================================================================
-- NEXT STEPS (vector embeddings)
--
-- To add vector embeddings, you'd need either:
--   a) A self-hosted embedding model on an internal URL (no auth headers needed)
--   b) A proxy that injects the Authorization header
--
-- pg-tikv's http_post(url, body, content_type) has no custom headers support,
-- so OpenAI API cannot be called directly from SQL.
--
-- Example with a local embedding service:
--
--   ALTER TABLE rag_documents ADD COLUMN embedding VECTOR(384);
--
--   -- In the CTE pipeline, add:
--   embedded AS (
--       SELECT
--           path, content, size,
--           (SELECT content FROM extensions.http_post(
--               'http://localhost:11434/api/embeddings',
--               json_build_object('model', 'all-minilm', 'prompt', fc.content)::text,
--               'application/json'
--           ))::jsonb -> 'embedding' AS vec
--       FROM file_contents fc
--   )
--   INSERT INTO rag_documents (file_path, content, file_size, tsv, embedding)
--   SELECT path, content, size,
--          to_tsvector('english', content),
--          vec::vector
--   FROM embedded;
-- ============================================================================
