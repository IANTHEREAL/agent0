-- PRD-D04: BYTEA low-level functions needed by uuidv7()

SELECT length(int8send(0::bigint)) AS int8send_len;
SELECT length(int4send(0)) AS int4send_len;
SELECT length(uuid_send(gen_random_uuid())) AS uuid_send_len;

SELECT int8send(72623859790382856::bigint) AS int8_bytes;
SELECT int4send(16909060) AS int4_bytes;
SELECT uuid_send('550e8400-e29b-41d4-a716-446655440000'::uuid) AS uuid_bytes;

SELECT set_bit('\x00'::bytea, 0, 1) AS set_msb;
SELECT set_bit('\x00'::bytea, 7, 1) AS set_lsb;
SELECT get_bit('\x80'::bytea, 0) AS get_msb;
SELECT get_bit('\x80'::bytea, 7) AS get_lsb;

-- uuidv7 critical sub-expression: 6 bytes timestamp (ms) payload
SELECT substring(int8send(1705312800000::bigint) from 3) AS ts_6bytes;

-- Overlay the 6-byte timestamp payload into a UUID byte array and show hex.
SELECT encode(
  overlay(
    uuid_send('550e8400-e29b-41d4-a716-446655440000'::uuid)
    placing substring(int8send(1705312800000::bigint) from 3)
    from 1 for 6
  ),
  'hex'
) AS overlay_hex;

