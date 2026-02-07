-- Regression: SELECT DISTINCT with ORDER BY referencing an input column name
-- (common ORM pattern) must work even when the output column is aliased.
SELECT DISTINCT "distinctAlias"."User_id" AS "ids_User_id"
FROM (VALUES (2), (1), (1)) AS "distinctAlias"("User_id")
ORDER BY "User_id" ASC
LIMIT 1;

