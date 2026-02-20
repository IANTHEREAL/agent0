CREATE TABLE plpgsql_test_data (id serial, name text, value integer);

CREATE FUNCTION insert_and_count() RETURNS integer AS $$
BEGIN
    INSERT INTO plpgsql_test_data (name, value) VALUES ('test1', 10);
    INSERT INTO plpgsql_test_data (name, value) VALUES ('test2', 20);
    RETURN 2;
END;
$$ LANGUAGE plpgsql;

SELECT insert_and_count();
SELECT count(*) FROM plpgsql_test_data;

CREATE FUNCTION do_perform() RETURNS void AS $$
BEGIN
    PERFORM 1;
    RETURN;
END;
$$ LANGUAGE plpgsql;

SELECT do_perform();

CREATE FUNCTION get_max_value() RETURNS integer AS $$
DECLARE
    max_val integer;
BEGIN
    SELECT INTO max_val max(value) FROM plpgsql_test_data;
    RETURN max_val;
END;
$$ LANGUAGE plpgsql;

SELECT get_max_value();

CREATE FUNCTION get_first_row() RETURNS text AS $$
DECLARE
    row_name text;
    row_value integer;
BEGIN
    SELECT INTO row_name, row_value name, value FROM plpgsql_test_data ORDER BY id LIMIT 1;
    RETURN row_name || ':' || row_value::text;
END;
$$ LANGUAGE plpgsql;

SELECT get_first_row();

CREATE FUNCTION sum_all_values() RETURNS integer AS $$
DECLARE
    total integer := 0;
    rec RECORD;
BEGIN
    FOR rec IN SELECT value FROM plpgsql_test_data ORDER BY id
    LOOP
        total := total + rec.value;
    END LOOP;
    RETURN total;
END;
$$ LANGUAGE plpgsql;

SELECT sum_all_values();

CREATE FUNCTION sum_range(n integer) RETURNS integer AS $$
DECLARE
    total integer := 0;
    i integer;
BEGIN
    FOR i IN 1..n LOOP
        total := total + i;
    END LOOP;
    RETURN total;
END;
$$ LANGUAGE plpgsql;

SELECT sum_range(5);
SELECT sum_range(10);

CREATE FUNCTION find_first_over(threshold integer) RETURNS text AS $$
DECLARE
    rec RECORD;
BEGIN
    FOR rec IN SELECT name, value FROM plpgsql_test_data ORDER BY id
    LOOP
        IF rec.value > threshold THEN
            RETURN rec.name;
        END IF;
    END LOOP;
    RETURN 'none';
END;
$$ LANGUAGE plpgsql;

SELECT find_first_over(15);

CREATE FUNCTION process_batch() RETURNS integer AS $$
DECLARE
    rec RECORD;
    processed integer := 0;
    current_value integer;
BEGIN
    FOR rec IN SELECT id, name, value FROM plpgsql_test_data ORDER BY id
    LOOP
        SELECT INTO current_value value FROM plpgsql_test_data WHERE id = rec.id;
        IF current_value IS NOT NULL THEN
            INSERT INTO plpgsql_test_data (name, value) VALUES ('processed_' || rec.name, current_value * 2);
            processed := processed + 1;
        END IF;
    END LOOP;
    RETURN processed;
END;
$$ LANGUAGE plpgsql;

SELECT process_batch();

DROP FUNCTION insert_and_count;
DROP FUNCTION do_perform;
DROP FUNCTION get_max_value;
DROP FUNCTION get_first_row;
DROP FUNCTION sum_all_values;
DROP FUNCTION sum_range;
DROP FUNCTION find_first_over;
DROP FUNCTION process_batch;
DROP TABLE plpgsql_test_data;
