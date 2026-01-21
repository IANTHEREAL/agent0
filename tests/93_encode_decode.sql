-- PRD-D05: encode() / decode() functions (hex/base64/escape)

SELECT encode('\x48656c6c6f'::bytea, 'hex') AS hex_hello;
SELECT decode('48656c6c6f', 'hex') AS hex_decoded;

SELECT encode('\x48656c6c6f'::bytea, 'base64') AS base64_hello;
SELECT decode('SGVsbG8=', 'base64') AS base64_decoded;

SELECT encode('\x48656c6c6f'::bytea, 'escape') AS escape_hello;
SELECT encode('\x5c001f207fff'::bytea, 'escape') AS escape_mixed;
SELECT decode('\\\000\037 \177\377', 'escape') AS escape_decoded;

SELECT decode(encode('\xdeadbeef'::bytea, 'hex'), 'hex') = '\xdeadbeef'::bytea AS roundtrip_hex;
SELECT decode(encode('\xdeadbeef'::bytea, 'escape'), 'escape') = '\xdeadbeef'::bytea AS roundtrip_escape;
SELECT decode(encode('hello'::bytea, 'base64'), 'base64') = 'hello'::bytea AS roundtrip_base64;

-- uuidv7 final path depends on encode(..., 'hex')::uuid accepting 32 hex digits.
SELECT encode('\x0189123456781234567890abcdef1234'::bytea, 'hex')::uuid AS uuid_cast;
SELECT encode('\x0189123456781234567890abcdef1234'::bytea, 'hex')::uuid IS NOT NULL AS uuid_not_null;

