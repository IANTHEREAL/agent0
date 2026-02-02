DROP TABLE IF EXISTS limit_pushdown_t;

CREATE TABLE limit_pushdown_t (
    id INT PRIMARY KEY,
    a INT NOT NULL
);

CREATE INDEX idx_limit_pushdown_a ON limit_pushdown_t(a);

