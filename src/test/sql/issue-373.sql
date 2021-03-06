CREATE TABLE foo AS SELECT 'bar' AS name;
CREATE INDEX foo_idx ON foo USING zombodb((foo.*)) WITH (url='localhost:9200/');

BEGIN;
SAVEPOINT a;
INSERT INTO foo (SELECT 'bar2' AS name);
ROLLBACK TO SAVEPOINT a;
COMMIT;

DROP TABLE foo;