-- #1579: pg_get_serial_sequence('', 'id') should error, not return NULL
SELECT pg_get_serial_sequence('', 'id');
