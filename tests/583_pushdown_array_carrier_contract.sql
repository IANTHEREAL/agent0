-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: array carriers cannot widen scalar output/equality contracts.

DROP TABLE IF EXISTS db9_cop_array_carrier_contract;

CREATE TABLE db9_cop_array_carrier_contract(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    numeric_arr NUMERIC[] NOT NULL,
    uuid_arr UUID[] NOT NULL,
    json_arr JSON[] NOT NULL,
    jsonb_arr JSONB[] NOT NULL,
    date_arr DATE[] NOT NULL,
    time_arr TIME[] NOT NULL
);
CREATE INDEX db9_cop_array_carrier_contract_n_idx
    ON db9_cop_array_carrier_contract(n);

INSERT INTO db9_cop_array_carrier_contract VALUES (
    1,
    20,
    ARRAY[1.5::numeric],
    ARRAY['00000000-0000-0000-0000-000000000001'::uuid],
    ARRAY['{"a":1}'::json],
    ARRAY['{"a":1}'::jsonb],
    ARRAY[DATE '2024-01-02'],
    ARRAY[TIME '03:04:05']
);

SET db9.enable_cop_pushdown = on;

\o /tmp/583_numeric_array_projection.txt
EXPLAIN SELECT numeric_arr FROM db9_cop_array_carrier_contract WHERE n = 20 LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output:" /tmp/583_numeric_array_projection.txt; then echo "numeric_array_projection_stays_local|0"; else echo "numeric_array_projection_stays_local|1"; fi

\o /tmp/583_uuid_array_projection.txt
EXPLAIN SELECT uuid_arr FROM db9_cop_array_carrier_contract WHERE n = 20 LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output:" /tmp/583_uuid_array_projection.txt; then echo "uuid_array_projection_stays_local|0"; else echo "uuid_array_projection_stays_local|1"; fi

\o /tmp/583_json_array_projection.txt
EXPLAIN SELECT json_arr FROM db9_cop_array_carrier_contract WHERE n = 20 LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output:" /tmp/583_json_array_projection.txt; then echo "json_array_projection_stays_local|0"; else echo "json_array_projection_stays_local|1"; fi

\o /tmp/583_jsonb_array_projection.txt
EXPLAIN SELECT jsonb_arr FROM db9_cop_array_carrier_contract WHERE n = 20 LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output:" /tmp/583_jsonb_array_projection.txt; then echo "jsonb_array_projection_stays_local|0"; else echo "jsonb_array_projection_stays_local|1"; fi

\o /tmp/583_numeric_array_eq.txt
EXPLAIN SELECT numeric_arr = ARRAY[1.5::numeric] FROM db9_cop_array_carrier_contract WHERE n = 20 LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output:" /tmp/583_numeric_array_eq.txt; then echo "numeric_array_eq_stays_local|0"; else echo "numeric_array_eq_stays_local|1"; fi

\o /tmp/583_date_array_eq.txt
EXPLAIN SELECT date_arr = ARRAY[DATE '2024-01-02'] FROM db9_cop_array_carrier_contract WHERE n = 20 LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output:" /tmp/583_date_array_eq.txt; then echo "date_array_eq_stays_local|0"; else echo "date_array_eq_stays_local|1"; fi

\o /tmp/583_time_array_eq.txt
EXPLAIN SELECT time_arr = ARRAY[TIME '03:04:05'] FROM db9_cop_array_carrier_contract WHERE n = 20 LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output:" /tmp/583_time_array_eq.txt; then echo "time_array_eq_stays_local|0"; else echo "time_array_eq_stays_local|1"; fi

\o /tmp/583_uuid_array_eq.txt
EXPLAIN SELECT uuid_arr = ARRAY['00000000-0000-0000-0000-000000000001'::uuid] FROM db9_cop_array_carrier_contract WHERE n = 20 LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output:" /tmp/583_uuid_array_eq.txt; then echo "uuid_array_eq_stays_local|0"; else echo "uuid_array_eq_stays_local|1"; fi

\o /tmp/583_jsonb_array_eq.txt
EXPLAIN SELECT jsonb_arr = ARRAY['{"a":1}'::jsonb] FROM db9_cop_array_carrier_contract WHERE n = 20 LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output:" /tmp/583_jsonb_array_eq.txt; then echo "jsonb_array_eq_stays_local|0"; else echo "jsonb_array_eq_stays_local|1"; fi

DROP TABLE db9_cop_array_carrier_contract;
