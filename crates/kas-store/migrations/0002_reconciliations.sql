ALTER TABLE resources ADD COLUMN observed_revision INTEGER NOT NULL DEFAULT -1;
ALTER TABLE resources ADD COLUMN claimed_revision INTEGER;
ALTER TABLE resources ADD COLUMN claim_driver_generation INTEGER;
