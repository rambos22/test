CREATE TABLE bytea_to_json (
  id serial8,
  data bytea
);

SELECT zdb.define_field_mapping('bytea_to_json', 'data', '{"type":"binary", "doc_values":true, "store":true}'); -- so we can search it
CREATE INDEX idxbytea_to_json ON bytea_to_json USING zombodb ((bytea_to_json.*));

INSERT INTO bytea_to_json (data) VALUES ('this is a test');
INSERT INTO bytea_to_json (data) VALUES ('this is a test 2');
INSERT INTO bytea_to_json (data) VALUES ('this is a test 3');
INSERT INTO bytea_to_json (data) VALUES ('this is a test 4');

-- finds "this is a test 3"
SELECT * FROM bytea_to_json WHERE bytea_to_json ==> dsl.script(
  format($$ doc['data'].value.utf8ToString().encodeBase64().equals('%s') $$, encode('this is a test 3', 'base64'))
);


DROP TABLE bytea_to_json;