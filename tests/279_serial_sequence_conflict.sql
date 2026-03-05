-- SERIAL should pick a non-conflicting implicit sequence name when a relation
-- with the default name already exists (PostgreSQL-compatible behavior).

-- cleanup
DROP TABLE IF EXISTS public.serial_conflict_a CASCADE;
DROP TABLE IF EXISTS public.serial_conflict_b CASCADE;
DROP TABLE IF EXISTS public.serial_conflict_c CASCADE;
DROP SEQUENCE IF EXISTS public.serial_conflict_a_id_seq;
DROP SEQUENCE IF EXISTS public.serial_conflict_a_id_seq1;
DROP SEQUENCE IF EXISTS public.serial_conflict_b_id_seq;
DROP SEQUENCE IF EXISTS public.serial_conflict_b_id_seq1;
DROP SEQUENCE IF EXISTS public.serial_conflict_b_id_seq2;
DROP SEQUENCE IF EXISTS public.serial_conflict_c_new_id_seq;
DROP SEQUENCE IF EXISTS public.serial_conflict_c_new_id_seq1;

-- case A: `_id_seq` exists -> SERIAL should use `_id_seq1`
CREATE SEQUENCE public.serial_conflict_a_id_seq START 100;
CREATE TABLE public.serial_conflict_a (id SERIAL PRIMARY KEY, payload TEXT);
INSERT INTO public.serial_conflict_a(payload) VALUES ('a');
SELECT 'case_a_old_seq_nextval=' || nextval('public.serial_conflict_a_id_seq')::text AS probe;

-- case B: `_id_seq` and `_id_seq1` both exist -> SERIAL should use `_id_seq2`
CREATE SEQUENCE public.serial_conflict_b_id_seq START 100;
CREATE SEQUENCE public.serial_conflict_b_id_seq1 START 200;
CREATE TABLE public.serial_conflict_b (id SERIAL PRIMARY KEY, payload TEXT);
INSERT INTO public.serial_conflict_b(payload) VALUES ('b');
SELECT 'case_b_old_seq_nextval=' || nextval('public.serial_conflict_b_id_seq')::text AS probe;
SELECT 'case_b_old_seq1_nextval=' || nextval('public.serial_conflict_b_id_seq1')::text AS probe;

-- case C: ALTER TABLE ADD COLUMN SERIAL conflict handling
CREATE TABLE public.serial_conflict_c(payload TEXT);
CREATE SEQUENCE public.serial_conflict_c_new_id_seq START 300;
ALTER TABLE public.serial_conflict_c ADD COLUMN new_id SERIAL;
INSERT INTO public.serial_conflict_c(payload) VALUES ('c');
SELECT 'case_c_old_seq_nextval=' || nextval('public.serial_conflict_c_new_id_seq')::text AS probe;

-- cleanup
DROP TABLE public.serial_conflict_a;
DROP TABLE public.serial_conflict_b;
DROP TABLE public.serial_conflict_c;
DROP SEQUENCE public.serial_conflict_a_id_seq;
DROP SEQUENCE public.serial_conflict_b_id_seq;
DROP SEQUENCE public.serial_conflict_b_id_seq1;
DROP SEQUENCE public.serial_conflict_c_new_id_seq;
