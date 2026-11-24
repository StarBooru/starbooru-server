-- Each account's tag blacklist: tag names (aliases resolved when saved) and
-- `rating:<rating>` entries whose posts reads leave out.
ALTER TABLE users ADD COLUMN blacklist TEXT[] NOT NULL DEFAULT '{}';

-- What new accounts start with, and guests get until they send their own.
-- NULL is none.
ALTER TABLE server_settings ADD COLUMN default_blacklist TEXT[];
