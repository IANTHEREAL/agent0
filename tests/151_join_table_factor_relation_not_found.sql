-- Regression: JOIN table-factor resolution must surface RelationNotFound.

DROP TABLE IF EXISTS join_tf_left;

CREATE TABLE join_tf_left(id INT);
INSERT INTO join_tf_left VALUES (1);

-- Missing relation in JOIN must not be collapsed into a generic "JOIN query could not be executed".
SELECT l.id FROM join_tf_left l JOIN join_tf_missing r ON l.id = r.id;

DROP TABLE join_tf_left;

