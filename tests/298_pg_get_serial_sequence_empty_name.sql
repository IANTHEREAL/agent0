-- #1579: pg_get_serial_sequence('', 'id') should error, not return NULL
SELECT pg_get_serial_sequence('', 'id');

-- #1579: pg_get_serial_sequence('', NULL) returns NULL (strict function semantics)
SELECT pg_get_serial_sequence('', NULL);
