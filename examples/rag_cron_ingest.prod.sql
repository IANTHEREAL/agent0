-- ============================================================================
-- RAG Ingest Pipeline — Batch Embedding + Parallel Workers
--
-- Optimized for 10,000+ files (10–100 KB each).
--
-- Key optimizations vs. naive sequential approach:
--
--   1. Batch Embedding: Groups N files per OpenAI API call (default 5),
--      reducing HTTP round-trips by 5×.
--
--   2. Parallel Workers: Hash-partitions rag_pending across W workers.
--      Each worker runs independently via pg_background_launch or
--      parallel shell sessions.
--
--   3. Combined effect: 10K files × 50 KB avg completes in ~5–10 min
--      (vs. ~100 min sequential).
--
-- Architecture:
--
--   Phase 1 — rag_scan_inbox():
--     Scans /inbox/ and known subdirectories for new files.
--     Inserts discovered paths into rag_pending staging table.
--     Must list each subdirectory explicitly (fs9() cannot accept variables).
--
--   Phase 2 — rag_ingest_batch(batch_size, embed_batch, worker_id, num_workers):
--     Claims files from its hash-partition of rag_pending.
--     For each sub-batch of embed_batch files:
--       read content → batch OpenAI embedding API → INSERT rag_documents.
--     Returns count of files processed.
--
--   Phase 3 — Parallel execution:
--     Option A (SQL): pg_background_launch spawns W workers
--     Option B (Shell): W parallel db9 processes (recommended for 10K+ files)
--
-- Prerequisites:
--   - pg-tikv with http extension
--   - Files uploaded to /inbox/ via `db9 fs cp` or SDK client.fs.write()
--   - Set your OpenAI API key in rag_ingest_batch() below
--
-- Usage:
--   1. Upload files:  db9 fs cp *.txt <db-id>:/inbox/
--   2. Seed this script:  db9 db seed <db-id> rag_cron_ingest.prod.sql
--   3. Scan inbox:   db9 db sql <db-id> -q "SELECT rag_scan_inbox()"
--   4. Parallel ingest — see PARALLEL EXECUTION section below
--   5. Monitor:  db9 db sql <db-id> -q "SELECT * FROM rag_ingest_status"
--   6. Search:   db9 db sql <db-id> -q "SELECT * FROM rag_search('query')"
--
-- ============================================================================
-- KNOWN LIMITATIONS (pg-tikv PL/pgSQL)
-- ============================================================================
--
-- 1. Variable / column name conflicts:
--    PL/pgSQL variables with the same name as table columns cause parse errors
--    in INSERT statements. FIX: Always prefix variables with v_.
--
-- 2. Dynamic table-valued function arguments:
--    fs9() cannot accept PL/pgSQL variables as arguments inside FOR loops.
--    FIX: Use a staging table approach with literal directory paths.
--
-- 3. EXECUTE (dynamic SQL) not supported.
--
-- 4. HTTP request limit per statement:
--    MAX_REQUESTS_PER_STATEMENT = 100. With embed_batch=5, each function call
--    can process up to 100 × 5 = 500 files. For larger sets, call repeatedly.
--
-- 5. OpenAI token limit:
--    text-embedding-3-small has an 8191 token limit (~32K chars). Large files
--    are truncated to 28000 chars before embedding. Full content is still
--    stored in rag_documents for FTS search.
--
-- 6. Batch embedding JSON size:
--    MAX_REQUEST_BYTES = 512 KiB. With embed_batch=5 and 28K-char truncation,
--    the JSON payload is ~150–170 KB. Safe for all file sizes. Reduce
--    embed_batch if you use a larger truncation limit.
--
-- 7. gRPC message size limit (4MB):
--    TiKV has a 4MB gRPC message size limit. Queries that scan rag_documents
--    (which stores full file content) may hit this. Use LIMIT on queries.
--
-- ============================================================================

-- ============================================================================
-- SETUP: Extensions
-- ============================================================================

CREATE EXTENSION IF NOT EXISTS http;

-- ============================================================================
-- CLEANUP: Clear old objects for fresh start
-- ============================================================================

DROP VIEW IF EXISTS rag_ingest_status;
DROP TABLE IF EXISTS rag_stage CASCADE;
DROP TABLE IF EXISTS rag_pending CASCADE;
DROP TABLE IF EXISTS rag_processed CASCADE;
DROP TABLE IF EXISTS rag_documents CASCADE;

-- ============================================================================
-- SETUP: Tables
-- ============================================================================

CREATE TABLE rag_pending (
    path         TEXT PRIMARY KEY,
    file_size    BIGINT
);

CREATE TABLE rag_processed (
    path         TEXT PRIMARY KEY,
    file_size    BIGINT,
    processed_at TIMESTAMP DEFAULT NOW()
);

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

-- Per-transaction scratch space for batch embedding.
-- Each concurrent worker writes with its own worker_id; rows are cleaned up
-- at the end of each sub-batch so the table stays small.
CREATE TABLE rag_stage (
    worker_id   INTEGER NOT NULL,
    seq         INTEGER NOT NULL,
    fpath       TEXT NOT NULL,
    fsize       BIGINT,
    content     TEXT,
    input_text  TEXT,
    PRIMARY KEY (worker_id, seq)
);

-- ============================================================================
-- PHASE 1: rag_scan_inbox() — Discover files
--
-- Scans /inbox/ and all known subdirectories for files not yet processed
-- or pending. Adds them to the rag_pending staging table.
--
-- NOTE: Each subdirectory must be listed explicitly because fs9() cannot
-- accept PL/pgSQL variable arguments (see limitation #2 above).
-- To add a new subdirectory, add another INSERT block below.
-- ============================================================================

CREATE OR REPLACE FUNCTION rag_scan_inbox()
RETURNS INTEGER AS $$
DECLARE
    v_count INTEGER;
BEGIN
    INSERT INTO rag_pending (path, file_size)
    SELECT f.path, f.size
    FROM extensions.fs9('/inbox/') f
    WHERE f.type = 'file'
      AND NOT EXISTS (SELECT 1 FROM rag_processed WHERE path = f.path)
      AND NOT EXISTS (SELECT 1 FROM rag_pending WHERE path = f.path);

    -- Add more INSERT blocks here for subdirectories, e.g.:
    -- INSERT INTO rag_pending (path, file_size)
    -- SELECT f.path, f.size
    -- FROM extensions.fs9('/inbox/subdir/') f
    -- WHERE f.type = 'file'
    --   AND NOT EXISTS (SELECT 1 FROM rag_processed WHERE path = f.path)
    --   AND NOT EXISTS (SELECT 1 FROM rag_pending WHERE path = f.path);

    SELECT count(*) INTO v_count FROM rag_pending;
    RETURN v_count;
END;
$$ LANGUAGE plpgsql;

-- ============================================================================
-- PHASE 2: rag_ingest_batch() — Batch embedding with parallel worker support
--
-- Processes up to batch_size files from this worker's hash-partition of
-- rag_pending.  Files are grouped into sub-batches of embed_batch and sent
-- to the OpenAI embeddings API in a single HTTP call per sub-batch.
--
-- Parameters:
--   p_batch_size   — max files to process in this call (default 100)
--   p_embed_batch  — files per API call (default 5; keep ≤ 8 for large files)
--   p_worker_id    — this worker's partition id (0-based, default 0)
--   p_num_workers  — total number of parallel workers (default 1)
--
-- Returns: number of files processed.
--
-- How partitioning works:
--   hashtext(path) distributes files deterministically across workers.
--   Each worker only sees its own slice — no locking or contention needed.
-- ============================================================================

CREATE OR REPLACE FUNCTION rag_ingest_batch(
    p_batch_size  INTEGER DEFAULT 100,
    p_embed_batch INTEGER DEFAULT 5,
    p_worker_id   INTEGER DEFAULT 0,
    p_num_workers INTEGER DEFAULT 1
) RETURNS INTEGER AS $$
DECLARE
    rec          RECORD;
    v_body       TEXT;
    v_input      TEXT;
    v_api        TEXT;
    v_vec        TEXT;
    v_inputs     TEXT;
    v_request    TEXT;
    v_total      INTEGER;
    v_sub_seq    INTEGER;
    v_iter       INTEGER;
    v_max_iters  INTEGER;
BEGIN
    v_total := 0;
    v_max_iters := (p_batch_size / p_embed_batch) + 1;

    DELETE FROM rag_stage WHERE worker_id = p_worker_id;

    FOR v_iter IN 0..v_max_iters LOOP
        IF v_total >= p_batch_size THEN
            EXIT;
        END IF;

        v_sub_seq := 0;

        FOR rec IN
            SELECT p.path AS fpath, p.file_size AS fsize
            FROM rag_pending p
            WHERE abs(hashtext(p.path)) % p_num_workers = p_worker_id
            ORDER BY p.path
            LIMIT p_embed_batch
        LOOP
            SELECT fs9_read(rec.fpath) INTO v_body;
            v_input := LEFT(v_body, 28000);

            INSERT INTO rag_stage (worker_id, seq, fpath, fsize, content, input_text)
            VALUES (p_worker_id, v_sub_seq,
                    rec.fpath, rec.fsize, v_body, v_input);

            v_sub_seq := v_sub_seq + 1;
        END LOOP;

        IF v_sub_seq = 0 THEN
            EXIT;
        END IF;

        SELECT json_agg(t.input_text)::text INTO v_inputs
        FROM (
            SELECT s.input_text
            FROM rag_stage s
            WHERE s.worker_id = p_worker_id
            ORDER BY s.seq
        ) t;

        v_request := '{"model":"text-embedding-3-small","input":'
                     || v_inputs || '}';

        SELECT INTO v_api content FROM extensions.http_post(
            'https://api.openai.com/v1/embeddings',
            v_request,
            'application/json',
            '{"Authorization":"Bearer sk-YOUR-KEY-HERE"}'
        );

        FOR rec IN
            SELECT s.seq, s.fpath, s.content, s.fsize
            FROM rag_stage s
            WHERE s.worker_id = p_worker_id
            ORDER BY s.seq
        LOOP
            SELECT INTO v_vec
                v_api::jsonb -> 'data' -> rec.seq ->> 'embedding';

            INSERT INTO rag_documents
                   (file_path, content, file_size, tsv, embedding)
            VALUES (rec.fpath, rec.content, rec.fsize,
                    to_tsvector('english', rec.content),
                    v_vec::vector(1536));

            INSERT INTO rag_processed (path, file_size)
            VALUES (rec.fpath, rec.fsize);

            DELETE FROM rag_pending WHERE path = rec.fpath;
        END LOOP;

        v_total := v_total + v_sub_seq;

        DELETE FROM rag_stage WHERE worker_id = p_worker_id;
    END LOOP;

    RETURN v_total;
END;
$$ LANGUAGE plpgsql;

-- ============================================================================
-- HELPER: Search (full-text keyword search)
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

-- ============================================================================
-- HELPER: Status view
-- ============================================================================

CREATE VIEW rag_ingest_status AS
SELECT
    (SELECT count(*) FROM rag_documents)   AS documents_ingested,
    (SELECT count(*) FROM rag_processed)   AS files_processed,
    (SELECT count(*) FROM rag_pending)     AS files_pending,
    (SELECT max(processed_at) FROM rag_processed) AS last_ingest_at;

-- ============================================================================
-- PARALLEL EXECUTION
--
-- After running  SELECT rag_scan_inbox();  choose one option below.
--
-- ── Option A: In-database parallelism (pg_background_launch) ──────────
--
-- Launches 5 background workers.  Each processes its hash-partition.
-- One call handles up to 500 files per worker (100 HTTP × 5 files/req).
-- Repeat until rag_ingest_status shows files_pending = 0.
--
--   SELECT pg_background_launch(
--       'SELECT rag_ingest_batch(500, 5, ' || w || ', 5)'
--   ) FROM generate_series(0, 4) AS w;
--
-- Monitor:
--   SELECT * FROM rag_ingest_status;
--
-- Repeat if files_pending > 0:
--   SELECT pg_background_launch(
--       'SELECT rag_ingest_batch(500, 5, ' || w || ', 5)'
--   ) FROM generate_series(0, 4) AS w;
--
-- ── Option B: Shell parallelism (recommended for 10K+ files) ──────────
--
-- Each shell process = separate connection = fresh HTTP request counter.
-- Workers loop until their partition is empty.
--
--   NUM_WORKERS=5
--   DB_ID=<your-db-id>
--   for w in $(seq 0 $((NUM_WORKERS - 1))); do
--     (
--       while true; do
--         n=$(db9 db sql "$DB_ID" -q \
--           "SELECT rag_ingest_batch(500, 5, $w, $NUM_WORKERS)" \
--           2>/dev/null | tr -d ' \n')
--         echo "worker $w: processed $n"
--         [ "$n" = "0" ] && break
--       done
--     ) &
--   done
--   wait
--   echo "=== done ==="
--
-- ── Option C: Single-threaded (small datasets or debugging) ───────────
--
--   SELECT rag_ingest_batch();
--
-- ============================================================================

-- ============================================================================
-- QUICK START (run after seed)
-- ============================================================================

-- Step 1: Discover files
-- SELECT rag_scan_inbox();

-- Step 2: Launch parallel ingest (5 workers, up to 500 files each)
-- SELECT pg_background_launch(
--     'SELECT rag_ingest_batch(500, 5, ' || w || ', 5)'
-- ) FROM generate_series(0, 4) AS w;

-- Step 3: Check progress
-- SELECT * FROM rag_ingest_status;

-- Step 4: Repeat step 2 if files_pending > 0

-- Step 5: Search
-- SELECT * FROM rag_search('your search terms');

-- ============================================================================
-- MONITORING
-- ============================================================================

-- Check ingest progress:
--   SELECT * FROM rag_ingest_status;
--
-- List processed files:
--   SELECT path, file_size, processed_at
--   FROM rag_processed ORDER BY processed_at DESC LIMIT 20;
--
-- List pending files:
--   SELECT path, file_size FROM rag_pending ORDER BY path LIMIT 20;
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
-- ============================================================================
-- SELF-HOSTED MODEL VARIANT (Ollama / vLLM)
--
-- For self-hosted models, modify rag_ingest_batch():
--   1. Change the http_post URL to your model endpoint
--   2. Adjust the JSON payload format for your model's batch API
--   3. Remove the Authorization header if not needed
--   4. Change VECTOR(1536) to match your model's dimension
--   5. Adjust embed_batch size based on your model's throughput
-- ============================================================================
