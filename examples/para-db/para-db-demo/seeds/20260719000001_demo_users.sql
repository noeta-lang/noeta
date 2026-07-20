-- Seed: re-runnable development data. Runs on every `noeta migrate seed` / `--seed`, each in its own
-- transaction, and is NOT tracked. Written with `INSERT OR IGNORE` so a second run is a no-op.
INSERT OR IGNORE INTO users (id, name) VALUES (100, 'Grace');
INSERT OR IGNORE INTO users (id, name) VALUES (101, 'Alan');
