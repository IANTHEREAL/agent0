-- #1578: Reject positional arguments after named arguments.

-- Table-valued function path (FROM clause analyzer).
SELECT * FROM generate_series(start := 1, 10);

-- Control: positional-only form must remain valid.
SELECT * FROM generate_series(1, 10) ORDER BY 1 LIMIT 2;

-- Scalar function path (expression analyzer).
SELECT length(str := 'hello', 1);

-- Control: positional-only form must remain valid.
SELECT length('hello');
