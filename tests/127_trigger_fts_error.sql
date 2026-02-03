-- Test that triggers using unsupported FTS functions produce clear error messages

-- Setup
DROP TABLE IF EXISTS test_fts_trigger;
CREATE TABLE test_fts_trigger (
    id SERIAL PRIMARY KEY,
    title TEXT,
    body TEXT,
    tsv TEXT
);

-- Try to create a trigger that uses tsvector_update_trigger (unsupported)
-- This should fail with a clear error message
CREATE OR REPLACE FUNCTION update_search_vector()
RETURNS TRIGGER AS $$
BEGIN
    NEW.tsv = tsvector_update_trigger(NEW.title, NEW.body);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER tsvector_update
    BEFORE INSERT OR UPDATE ON test_fts_trigger
    FOR EACH ROW EXECUTE FUNCTION update_search_vector();

-- This INSERT should trigger the FTS error
INSERT INTO test_fts_trigger (title, body) VALUES ('Hello', 'World');

-- Cleanup
DROP TABLE IF EXISTS test_fts_trigger;
