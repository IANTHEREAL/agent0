-- Test for ELSIF handling issues

DROP FUNCTION IF EXISTS test_elsif(integer);
CREATE FUNCTION test_elsif(n integer) RETURNS text AS $$
BEGIN
    IF n > 0 THEN
        RETURN 'positive';
    ELSIF n < 0 THEN
        RETURN 'negative';
    ELSE
        RETURN 'zero';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT test_elsif(5) AS pos;
SELECT test_elsif(-3) AS neg;
SELECT test_elsif(0) AS zero;

DROP FUNCTION IF EXISTS test_multiple_elsif(integer);
CREATE FUNCTION test_multiple_elsif(n integer) RETURNS text AS $$
BEGIN
    IF n >= 90 THEN
        RETURN 'A';
    ELSIF n >= 80 THEN
        RETURN 'B';
    ELSIF n >= 70 THEN
        RETURN 'C';
    ELSIF n >= 60 THEN
        RETURN 'D';
    ELSE
        RETURN 'F';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT test_multiple_elsif(95) AS grade_a;
SELECT test_multiple_elsif(85) AS grade_b;
SELECT test_multiple_elsif(75) AS grade_c;
SELECT test_multiple_elsif(65) AS grade_d;
SELECT test_multiple_elsif(55) AS grade_f;

DROP FUNCTION IF EXISTS test_elsif(integer);
DROP FUNCTION IF EXISTS test_multiple_elsif(integer);
