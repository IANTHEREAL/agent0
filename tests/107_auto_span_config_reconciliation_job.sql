-- PostgreSQL compatible tests from auto_span_config_reconciliation_job
-- 4 tests

-- Test 1: statement (line 2)
-- CockroachDB automatic jobs and span config reconciliation are not available in
-- PostgreSQL. To preserve the intent of the test (job appears + can be
-- cancelled), we simulate minimal job metadata locally.
CREATE TABLE automatic_jobs (
    job_id BIGSERIAL PRIMARY KEY,
    job_type TEXT NOT NULL,
    status TEXT NOT NULL
);

-- SET CLUSTER SETTING spanconfig.reconciliation_job.enabled = true;
INSERT INTO automatic_jobs (job_type, status)
VALUES ('AUTO SPAN CONFIG RECONCILIATION', 'running');

SELECT job_id
FROM automatic_jobs
WHERE job_type = 'AUTO SPAN CONFIG RECONCILIATION'
ORDER BY job_id DESC
LIMIT 1
\gset

-- Test 2: query (line 7)
-- SELECT count(*), job_type, status FROM [SHOW AUTOMATIC JOBS] WHERE job_type = 'AUTO SPAN CONFIG RECONCILIATION' GROUP BY (job_type, status)
SELECT count(*), job_type, status
FROM automatic_jobs
WHERE job_type = 'AUTO SPAN CONFIG RECONCILIATION'
GROUP BY job_type, status;

-- Test 3: statement (line 24)
-- CANCEL JOB $job_id
UPDATE automatic_jobs SET status = 'canceled' WHERE job_id = :job_id;

-- Test 4: query (line 28)
-- SELECT status FROM [SHOW AUTOMATIC JOBS] WHERE job_id = $job_id
SELECT status FROM automatic_jobs WHERE job_id = :job_id;
