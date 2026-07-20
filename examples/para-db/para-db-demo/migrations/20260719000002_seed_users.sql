-- A second migration with two statements, applied together in one transaction.
INSERT INTO users (id, name) VALUES (1, 'Ada');
INSERT INTO users (id, name) VALUES (2, 'Bob');
