-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: CHR error surfaces match PostgreSQL on/off.

DROP TABLE IF EXISTS db9_cop_chr_error_smoke;

CREATE TABLE db9_cop_chr_error_smoke(
    id INT PRIMARY KEY,
    code INT NOT NULL
);

INSERT INTO db9_cop_chr_error_smoke VALUES
    (1, 55296),
    (2, 1114112);

SET db9.enable_cop_pushdown = on;
SELECT chr(code) FROM db9_cop_chr_error_smoke WHERE id = 1;
SELECT chr(code) FROM db9_cop_chr_error_smoke WHERE id = 2;

SET db9.enable_cop_pushdown = off;
SELECT chr(code) FROM db9_cop_chr_error_smoke WHERE id = 1;
SELECT chr(code) FROM db9_cop_chr_error_smoke WHERE id = 2;

DROP TABLE db9_cop_chr_error_smoke;
