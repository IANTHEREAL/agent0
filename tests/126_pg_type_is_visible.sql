-- pg_type_is_visible function (SQLAlchemy enum compatibility)

SELECT pg_type_is_visible(23);
SELECT pg_type_is_visible(25);
SELECT pg_type_is_visible(NULL);

CREATE TYPE test_mood_visible AS ENUM ('happy', 'sad', 'neutral');

SELECT pg_type_is_visible(t.oid) AS is_visible
FROM pg_type t 
WHERE t.typname = 'test_mood_visible';

DROP TYPE test_mood_visible;
