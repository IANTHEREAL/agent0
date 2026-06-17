-- ---------------------------------------------------------------------------
-- This file is a partial, adapted derivative of PostgreSQL's
-- src/test/regress/sql/test_setup.sql (tag REL_17_10). It is redistributed here
-- under the PostgreSQL License; the original copyright notice is retained below
-- as required by that license.
--
-- PostgreSQL Database Management System
-- (formerly known as Postgres, then as Postgres95)
--
-- Portions Copyright (c) 1996-2024, PostgreSQL Global Development Group
-- Portions Copyright (c) 1994, The Regents of the University of California
--
-- Permission to use, copy, modify, and distribute this software and its
-- documentation for any purpose, without fee, and without a written agreement
-- is hereby granted, provided that the above copyright notice and this
-- paragraph and the following two paragraphs appear in all copies.
--
-- IN NO EVENT SHALL THE UNIVERSITY OF CALIFORNIA BE LIABLE TO ANY PARTY FOR
-- DIRECT, INDIRECT, SPECIAL, INCIDENTAL, OR CONSEQUENTIAL DAMAGES, INCLUDING
-- LOST PROFITS, ARISING OUT OF THE USE OF THIS SOFTWARE AND ITS DOCUMENTATION,
-- EVEN IF THE UNIVERSITY OF CALIFORNIA HAS BEEN ADVISED OF THE POSSIBILITY OF
-- SUCH DAMAGE.
--
-- THE UNIVERSITY OF CALIFORNIA SPECIFICALLY DISCLAIMS ANY WARRANTIES, INCLUDING,
-- BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
-- A PARTICULAR PURPOSE. THE SOFTWARE PROVIDED HEREUNDER IS ON AN "AS IS" BASIS,
-- AND THE UNIVERSITY OF CALIFORNIA HAS NO OBLIGATIONS TO PROVIDE MAINTENANCE,
-- SUPPORT, UPDATES, ENHANCEMENTS, OR MODIFICATIONS.
-- ---------------------------------------------------------------------------

-- Adapted, portable subset of PG regress test_setup.sql (REL_17_10).
-- Goal: identical small fixtures on BOTH the PG 17.10 oracle and db9, so any
-- per-statement divergence in the scalar/expression corpus is a real db9 gap,
-- not a missing-fixture artifact.
--
-- DELIBERATELY OMITTED from upstream test_setup.sql (non-portable / not needed
-- by the scalar batch), each a candidate for later adaptation:
--   * \getenv / \set :libdir interpolation        (psql client machinery)
--   * CREATE FUNCTION ... LANGUAGE C  (regress.so) (not built in stock image)
--   * COPY onek/tenk1/... FROM :filename           (server-side data files)
--   * CREATE TABLESPACE regress_tblspace           (filesystem-dependent)
-- The big data-file tables (onek, tenk1, person, emp, ...) are loaded
-- separately by the harness via client-side COPY in a later batch.

CREATE TABLE CHAR_TBL(f1 char(4));
INSERT INTO CHAR_TBL (f1) VALUES ('a'), ('ab'), ('abcd'), ('abcd    ');

CREATE TABLE FLOAT8_TBL(f1 float8);
INSERT INTO FLOAT8_TBL(f1) VALUES
  ('0.0'), ('-34.84'), ('-1004.30'),
  ('-1.2345678901234e+200'), ('-1.2345678901234e-200');

CREATE TABLE INT2_TBL(f1 int2);
INSERT INTO INT2_TBL(f1) VALUES
  ('0   '), ('  1234 '), ('    -1234'), ('32767'), ('-32767');

CREATE TABLE INT4_TBL(f1 int4);
INSERT INTO INT4_TBL(f1) VALUES
  ('   0  '), ('123456     '), ('    -123456'), ('2147483647'), ('-2147483647');

CREATE TABLE INT8_TBL(q1 int8, q2 int8);
INSERT INTO INT8_TBL VALUES
  ('  123   ','  456'),
  ('123   ','4567890123456789'),
  ('4567890123456789','123'),
  (+4567890123456789,'4567890123456789'),
  ('+4567890123456789','-4567890123456789');

CREATE TABLE TEXT_TBL (f1 text);
INSERT INTO TEXT_TBL VALUES ('doh!'), ('hi de ho neighbor');

CREATE TABLE VARCHAR_TBL(f1 varchar(4));
INSERT INTO VARCHAR_TBL (f1) VALUES ('a'), ('ab'), ('abcd'), ('abcd    ');
