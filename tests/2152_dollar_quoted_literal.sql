-- Issue #2152: bare dollar-quoted string literal should parse in a top-level SELECT list.

SELECT $$x$$ AS v;
