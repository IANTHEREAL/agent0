-- ============================================================================
-- RAG Ingest Pipeline via PL/pgSQL + pg_cron + OpenAI Embeddings
--
-- Demonstrates the full pg-tikv agentic SQL stack:
--   PL/pgSQL control flow  (FOR loop, SELECT INTO, SQL execution)
--   fs9 file system         (list files, read content)
--   http extension          (call OpenAI API with custom Authorization header)
--   vector built-in         (store & query embeddings)
--   full-text search        (GIN-indexed tsvector)
--   pg_cron                 (scheduled background execution)
--
-- Pattern:
--   cron fires every minute → calls rag_ingest_batch()
--   → FOR loop over unprocessed files in /inbox/
--   → read content via fs9_read
--   → call OpenAI embeddings API via http_post with Authorization header
--   → INSERT into rag_documents (content + vector + tsvector)
--   → INSERT into rag_processed (tracking)
--   → when /inbox/ is empty, function returns 0
--
-- Prerequisites:
--   - pg-tikv with http + pg_cron extensions
--   - Files uploaded to /inbox/ via `db9 fs cp` or SDK client.fs.write()
--   - Set your OpenAI API key below
--
-- Usage:
--   1. Upload files:  db9 fs cp *.txt <db-id>:/inbox/
--   2. Run this script to set up tables + function + cron job
--   3. Monitor:  SELECT * FROM rag_ingest_status;
--   4. Search:   SELECT * FROM rag_search('your query here');
-- ============================================================================

-- ============================================================================
-- SETUP: Extensions
-- ============================================================================

CREATE EXTENSION IF NOT EXISTS http;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- ============================================================================
-- CONFIG: Set your OpenAI API key here
-- ============================================================================
-- Replace 'sk-YOUR-KEY-HERE' with your actual key, or use a self-hosted model.

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

-- GIN index for full-text search
SET tipg.use_optimizer = off;
CREATE INDEX idx_rag_fts ON rag_documents USING GIN (tsv);
SET tipg.use_optimizer = on;

-- ============================================================================
-- CORE: rag_ingest_batch() — PL/pgSQL function
--
-- Iterates over unprocessed files in /inbox/ using a FOR loop,
-- reads each file, calls the OpenAI embedding API, and stores
-- the result with both vector embedding and tsvector for hybrid search.
-- ============================================================================

CREATE OR REPLACE FUNCTION rag_ingest_batch(batch_size INTEGER DEFAULT 10)
RETURNS INTEGER AS $$
DECLARE
    rec        RECORD;
    file_body  TEXT;
    api_result TEXT;
    vec_text   TEXT;
    processed  INTEGER;
BEGIN
    processed := 0;

    FOR rec IN
        SELECT f.path, f.size
        FROM extensions.fs9('/inbox/') f
        LEFT JOIN rag_processed p ON p.path = f.path
        WHERE f.type = 'file'
          AND p.path IS NULL
        ORDER BY f.path
    LOOP
        SELECT fs9_read(rec.path) INTO file_body;

        SELECT INTO api_result content FROM extensions.http_post(
            'https://api.openai.com/v1/embeddings',
            json_build_object(
                'model', 'text-embedding-3-small',
                'input', file_body
            )::text,
            'application/json',
            '{"Authorization":"Bearer sk-YOUR-KEY-HERE"}'
        );

        SELECT INTO vec_text
            api_result::jsonb -> 'data' -> 0 ->> 'embedding';

        INSERT INTO rag_documents (file_path, content, file_size, tsv, embedding)
        VALUES (
            rec.path,
            file_body,
            rec.size,
            to_tsvector('english', file_body),
            vec_text::vector(1536)
        );

        INSERT INTO rag_processed (path, file_size)
        VALUES (rec.path, rec.size);

        processed := processed + 1;
        IF processed >= batch_size THEN
            EXIT;
        END IF;
    END LOOP;

    RETURN processed;
END;
$$ LANGUAGE plpgsql;

-- ============================================================================
-- HELPER: Search (hybrid FTS + vector)
-- ============================================================================

-- Full-text keyword search
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
-- CRON: Schedule the ingest function
--
-- Runs every minute. Each run calls rag_ingest_batch() which processes
-- all new files. When /inbox/ has no new files, the function returns 0.
-- ============================================================================

SELECT cron.unschedule('rag_ingest') WHERE EXISTS (
    SELECT 1 FROM cron.job WHERE jobname = 'rag_ingest'
);

SELECT cron.schedule('rag_ingest', '* * * * *',
    'SELECT rag_ingest_batch()'
);

-- ============================================================================
-- VERIFY: Show scheduled job
-- ============================================================================

SELECT jobid, jobname, schedule, command
FROM cron.job
WHERE jobname = 'rag_ingest';

-- ============================================================================
-- MANUAL TEST: Run the pipeline once without waiting for cron
-- ============================================================================

-- SELECT rag_ingest_batch();

-- ============================================================================
-- SELF-HOSTED MODEL VARIANT (Ollama / vLLM)
--
-- For self-hosted models, modify the function body:
--   1. Change the http_post URL to your model endpoint
--   2. Adjust the JSON payload format for your model
--   3. Remove the Authorization header if not needed
--   4. Change VECTOR(1536) to match your model's dimension
--
-- Example Ollama modification (inside the FOR loop):
--
--   SELECT INTO api_resp c.content FROM extensions.http_post(
--       'http://localhost:11434/api/embeddings',
--       json_build_object('model', 'all-minilm', 'prompt', content)::text,
--       'application/json'
--   ) c;
--
--   SELECT INTO vec_text api_resp::jsonb ->> 'embedding';
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
--   SELECT runid, jobid, status, return_message, start_time, end_time
--   FROM cron.job_run_details
--   WHERE jobid = (SELECT jobid FROM cron.job WHERE jobname = 'rag_ingest')
--   ORDER BY runid DESC LIMIT 10;
--
-- Search documents (FTS):
--   SELECT * FROM rag_search('your search terms');
--
-- Search documents (vector similarity — requires embedding the query):
--   SELECT d.file_path, LEFT(d.content, 200) AS snippet,
--          d.embedding <-> query_vec AS distance
--   FROM rag_documents d,
--        LATERAL (
--            SELECT ((c.content::jsonb -> 'data' -> 0 ->> 'embedding')::vector) AS query_vec
--            FROM extensions.http_post(
--                'https://api.openai.com/v1/embeddings',
--                json_build_object('model', 'text-embedding-3-small', 'input', 'your query')::text,
--                'application/json',
--                '{"Authorization":"Bearer sk-YOUR-KEY-HERE"}'
--            ) c
--        ) q
--   ORDER BY distance
--   LIMIT 10;
--
-- Stop the pipeline:
--   SELECT cron.unschedule('rag_ingest');
