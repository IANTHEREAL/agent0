-- PG_PARITY: unique-violation DETAIL renders bytea values like PostgreSQL.
-- Lock the user-visible DETAIL formatting that now follows Value::Display.

DROP TABLE IF EXISTS unique_detail_bytea_insert;
CREATE TABLE unique_detail_bytea_insert (
    id INT PRIMARY KEY,
    payload BYTEA,
    CONSTRAINT unique_detail_bytea_insert_payload_key UNIQUE (payload)
);

INSERT INTO unique_detail_bytea_insert VALUES (1, decode('0001ff', 'hex'));
INSERT INTO unique_detail_bytea_insert VALUES (2, decode('0001ff', 'hex'));

DROP TABLE IF EXISTS unique_detail_bytea_index;
CREATE TABLE unique_detail_bytea_index (
    id INT PRIMARY KEY,
    payload BYTEA
);

INSERT INTO unique_detail_bytea_index VALUES
    (1, decode('5c78', 'hex')),
    (2, decode('5c78', 'hex'));

CREATE UNIQUE INDEX unique_detail_bytea_payload_idx ON unique_detail_bytea_index (payload);
