DROP TABLE IF EXISTS articles;
CREATE TABLE articles (
    id SERIAL PRIMARY KEY,
    title TEXT,
    tags TEXT[]
);

CREATE INDEX idx_tags_gin ON articles USING GIN (tags);

INSERT INTO articles (title, tags) VALUES ('Rust Guide', ARRAY['rust', 'programming']);
INSERT INTO articles (title, tags) VALUES ('TiKV Intro', ARRAY['tikv', 'database', 'rust']);
INSERT INTO articles (title, tags) VALUES ('PostgreSQL', ARRAY['postgres', 'database']);
INSERT INTO articles (title, tags) VALUES ('SQL Basics', ARRAY['sql', 'database']);

SELECT id, title FROM articles WHERE tags @> ARRAY['rust'] ORDER BY id;

SELECT id, title FROM articles WHERE tags @> ARRAY['database'] ORDER BY id;

SELECT id, title FROM articles WHERE tags @> ARRAY['rust', 'programming'] ORDER BY id;

SELECT id, title FROM articles WHERE tags @> ARRAY['nonexistent'] ORDER BY id;

DROP TABLE articles;
