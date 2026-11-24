-- Checks for new releases and automatic updates. NULL follows the environment variable.
ALTER TABLE server_settings ADD COLUMN update_check BOOLEAN;
ALTER TABLE server_settings ADD COLUMN auto_update BOOLEAN;
ALTER TABLE server_settings ADD COLUMN auto_update_hour SMALLINT
    CHECK (auto_update_hour BETWEEN 0 AND 23);

-- What the last checks found, and the update under way or the last one's
-- result, so a restart (an update's own included) loses neither.
CREATE TABLE update_state (
    id          BOOLEAN     PRIMARY KEY DEFAULT true CHECK (id),
    -- The newest release from the last check that worked; NULL when there is none.
    latest      JSONB,
    checked_at  TIMESTAMPTZ,
    -- Why the last check failed; NULL when it worked.
    check_error TEXT,
    run         JSONB
);
INSERT INTO update_state DEFAULT VALUES;
