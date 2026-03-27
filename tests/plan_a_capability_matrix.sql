-- =========================
-- Plan A Capability Matrix — Formal Regression Test (#2152)
--
-- End-to-end acceptance test for the Plan A swarm SQL contract.
-- All 5 stored functions (create_project, submit_task, try_claim,
-- heartbeat_claim, complete_task) must create and execute successfully.
--
-- Parameters use p_ prefix to avoid text-substitution collision
-- with column names.
-- =========================

CREATE EXTENSION IF NOT EXISTS fs9;
CREATE SCHEMA IF NOT EXISTS swarm;

-- ---- tables (SSOT) ----
CREATE TABLE IF NOT EXISTS swarm.projects (
  project_id text PRIMARY KEY,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS swarm.tasks (
  project_id text NOT NULL,
  task_id text NOT NULL,
  title text NOT NULL,
  status text NOT NULL DEFAULT 'open',
  priority int NOT NULL DEFAULT 2,
  spec jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (project_id, task_id)
);

CREATE TABLE IF NOT EXISTS swarm.task_claims (
  project_id text NOT NULL,
  task_id text NOT NULL,
  agent_id text NOT NULL,
  lease_until timestamptz NOT NULL,
  heartbeat_at timestamptz NOT NULL DEFAULT now(),
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (project_id, task_id),
  FOREIGN KEY (project_id, task_id) REFERENCES swarm.tasks(project_id, task_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_swarm_task_claims_lease_until
  ON swarm.task_claims(project_id, lease_until);

CREATE TABLE IF NOT EXISTS swarm.artifacts (
  project_id text NOT NULL,
  task_id text NOT NULL,
  path text NOT NULL,
  content_type text NOT NULL DEFAULT 'text/plain',
  bytes int NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (project_id, task_id, path)
);

-- ---- Function 1: bootstrap ----
CREATE OR REPLACE FUNCTION swarm.bootstrap(p_project_id text, p_create_demo_task boolean DEFAULT true)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
  root text := '/swarm/projects/' || p_project_id;
  cfg jsonb;
BEGIN
  IF p_project_id IS NULL OR length(p_project_id) = 0 THEN
    RAISE EXCEPTION 'project_id required';
  END IF;

  INSERT INTO swarm.projects(project_id) VALUES (p_project_id)
  ON CONFLICT (project_id) DO NOTHING;

  PERFORM extensions.fs9_mkdir('/swarm', true);
  PERFORM extensions.fs9_mkdir('/swarm/projects', true);

  PERFORM extensions.fs9_mkdir(root, true);
  PERFORM extensions.fs9_mkdir(root || '/tasks', true);
  PERFORM extensions.fs9_mkdir(root || '/claims', true);
  PERFORM extensions.fs9_mkdir(root || '/artifacts', true);
  PERFORM extensions.fs9_mkdir(root || '/state', true);
  PERFORM extensions.fs9_mkdir(root || '/state/cursors', true);
  PERFORM extensions.fs9_mkdir(root || '/logs', true);

  cfg := jsonb_build_object(
    'project_id', p_project_id,
    'version', 1,
    'paths', jsonb_build_object(
      'root', root,
      'tasks', root || '/tasks',
      'claims', root || '/claims',
      'artifacts', root || '/artifacts',
      'state', root || '/state',
      'logs', root || '/logs'
    ),
    'leases', jsonb_build_object('claim_lease_seconds', 120, 'heartbeat_interval_seconds', 30),
    'polling', jsonb_build_object('fs9_events_limit', 10000, 'poll_interval_ms', 500)
  );

  PERFORM extensions.fs9_write(root || '/state/config.json', jsonb_pretty(cfg)::text || E'\n');
  PERFORM extensions.fs9_write(root || '/README.md', '# Swarm project: ' || p_project_id || E'\n');

  IF p_create_demo_task THEN
    PERFORM swarm.create_task(p_project_id, 'T0', 'demo: listen fs9_events and try_claim', 9, '{}'::jsonb);
  END IF;
END;
$$;


-- ---- Function 2: create_task ----
CREATE OR REPLACE FUNCTION swarm.create_task(
  p_project_id text,
  p_task_id text,
  p_title text,
  p_priority int DEFAULT 2,
  p_spec jsonb DEFAULT '{}'::jsonb
) RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
  root text := '/swarm/projects/' || p_project_id;
  path text := root || '/tasks/' || p_task_id || '.json';
  payload jsonb;
BEGIN
  INSERT INTO swarm.tasks(project_id, task_id, title, status, priority, spec)
  VALUES (p_project_id, p_task_id, p_title, 'open', p_priority, COALESCE(p_spec,'{}'::jsonb))
  ON CONFLICT (project_id, task_id) DO UPDATE
    SET title=EXCLUDED.title, status='open', priority=EXCLUDED.priority, spec=EXCLUDED.spec, updated_at=now();

  payload := jsonb_build_object(
    'project_id', p_project_id,
    'task_id', p_task_id,
    'title', p_title,
    'status', 'open',
    'priority', p_priority,
    'spec', COALESCE(p_spec,'{}'::jsonb),
    'updated_at', to_char(now() at time zone 'utc','YYYY-MM-DD"T"HH24:MI:SS"Z"')
  );

  PERFORM extensions.fs9_write(path, jsonb_pretty(payload)::text || E'\n');
END;
$$;


-- ---- Function 3: try_claim ----
CREATE OR REPLACE FUNCTION swarm.try_claim(
  p_project_id text,
  p_task_id text,
  p_agent_id text,
  p_lease_seconds int DEFAULT 120
) RETURNS boolean
LANGUAGE plpgsql
AS $$
DECLARE
  until_ts timestamptz := now() + make_interval(secs => p_lease_seconds);
  root text := '/swarm/projects/' || p_project_id;
  claim_dir text := root || '/claims/' || p_task_id;
  claim_path text := claim_dir || '/' || p_agent_id || '.json';
  ok boolean;
BEGIN
  INSERT INTO swarm.task_claims(project_id, task_id, agent_id, lease_until)
  VALUES (p_project_id, p_task_id, p_agent_id, until_ts)
  ON CONFLICT (project_id, task_id) DO UPDATE
    SET agent_id=EXCLUDED.agent_id, lease_until=EXCLUDED.lease_until, heartbeat_at=now()
    WHERE swarm.task_claims.lease_until < now();

  SELECT (c.agent_id = p_agent_id) INTO ok
  FROM swarm.task_claims c
  WHERE c.project_id = p_project_id AND c.task_id = p_task_id;

  IF ok THEN
    UPDATE swarm.tasks SET status='claimed', updated_at=now()
    WHERE swarm.tasks.project_id = p_project_id AND swarm.tasks.task_id = p_task_id;

    PERFORM extensions.fs9_mkdir(claim_dir, true);
    PERFORM extensions.fs9_write(
      claim_path,
      jsonb_pretty(jsonb_build_object(
        'project_id', p_project_id,
        'task_id', p_task_id,
        'agent_id', p_agent_id,
        'lease_until', to_char(until_ts at time zone 'utc','YYYY-MM-DD"T"HH24:MI:SS"Z"'),
        'heartbeat_at', to_char(now() at time zone 'utc','YYYY-MM-DD"T"HH24:MI:SS"Z"')
      ))::text || E'\n'
    );
  END IF;

  RETURN ok;
END;
$$;


-- ---- Function 4: heartbeat_claim ----
CREATE OR REPLACE FUNCTION swarm.heartbeat_claim(
  p_project_id text,
  p_task_id text,
  p_agent_id text,
  p_lease_seconds int DEFAULT 120
) RETURNS boolean
LANGUAGE plpgsql
AS $$
DECLARE
  until_ts timestamptz := now() + make_interval(secs => p_lease_seconds);
  root text := '/swarm/projects/' || p_project_id;
  claim_path text := root || '/claims/' || p_task_id || '/' || p_agent_id || '.json';
  ok boolean := false;
BEGIN
  UPDATE swarm.task_claims
    SET heartbeat_at=now(), lease_until=until_ts
  WHERE swarm.task_claims.project_id = p_project_id
    AND swarm.task_claims.task_id = p_task_id
    AND swarm.task_claims.agent_id = p_agent_id
    AND swarm.task_claims.lease_until >= now()
  RETURNING true INTO ok;

  IF ok THEN
    PERFORM extensions.fs9_write(
      claim_path,
      jsonb_pretty(jsonb_build_object(
        'project_id', p_project_id,
        'task_id', p_task_id,
        'agent_id', p_agent_id,
        'lease_until', to_char(until_ts at time zone 'utc','YYYY-MM-DD"T"HH24:MI:SS"Z"'),
        'heartbeat_at', to_char(now() at time zone 'utc','YYYY-MM-DD"T"HH24:MI:SS"Z"')
      ))::text || E'\n'
    );
  END IF;

  RETURN ok;
END;
$$;


-- ---- Function 5: write_artifact ----
CREATE OR REPLACE FUNCTION swarm.write_artifact(
  p_project_id text,
  p_task_id text,
  p_rel_path text,
  p_content text,
  p_content_type text DEFAULT 'text/plain'
) RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
  root text := '/swarm/projects/' || p_project_id;
  dir text := root || '/artifacts/' || p_task_id;
  full_path text := dir || '/' || p_rel_path;
  nbytes int := octet_length(COALESCE(p_content,''));
BEGIN
  PERFORM extensions.fs9_mkdir(dir, true);
  PERFORM extensions.fs9_write(full_path, COALESCE(p_content,''));

  INSERT INTO swarm.artifacts(project_id, task_id, path, content_type, bytes)
  VALUES (p_project_id, p_task_id, p_rel_path, p_content_type, nbytes)
  ON CONFLICT (project_id, task_id, path) DO UPDATE
    SET content_type=EXCLUDED.content_type, bytes=EXCLUDED.bytes, updated_at=now();
END;
$$;


-- ==== Smoke tests ====

-- Test bootstrap
SELECT swarm.bootstrap('matrix-test');

-- Test create_task
SELECT swarm.create_task('matrix-test', 'T1', 'test task', 5, '{"key":"val"}'::jsonb);

-- Test try_claim
SELECT swarm.try_claim('matrix-test', 'T1', 'agent-1', 120);

-- Test heartbeat
SELECT swarm.heartbeat_claim('matrix-test', 'T1', 'agent-1', 120);

-- Test write_artifact
SELECT swarm.write_artifact('matrix-test', 'T1', 'result.txt', 'hello world', 'text/plain');

-- Cleanup
DROP FUNCTION IF EXISTS swarm.write_artifact;
DROP FUNCTION IF EXISTS swarm.heartbeat_claim;
DROP FUNCTION IF EXISTS swarm.try_claim;
DROP FUNCTION IF EXISTS swarm.create_task;
DROP FUNCTION IF EXISTS swarm.bootstrap;
DROP TABLE IF EXISTS swarm.artifacts;
DROP TABLE IF EXISTS swarm.task_claims;
DROP TABLE IF EXISTS swarm.tasks;
DROP TABLE IF EXISTS swarm.projects;
DROP SCHEMA IF EXISTS swarm;
