-- psql 18 queries datlocprovider from pg_database (#1446)
SELECT datlocprovider FROM pg_catalog.pg_database LIMIT 1;
SELECT datlocprovider, datlocale FROM pg_catalog.pg_database WHERE datname = current_database();
