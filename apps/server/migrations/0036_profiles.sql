-- Profiles. The username stays the handle; these are what others see of an
-- account on a public server. NULL is none.
ALTER TABLE users ADD COLUMN display_name TEXT CHECK (char_length(display_name) BETWEEN 1 AND 40);
ALTER TABLE users ADD COLUMN about TEXT CHECK (char_length(about) BETWEEN 1 AND 500);
-- The SHA-256 of the 256 px JPEG in <data>/avatar/; the 64 px one has the
-- same name in <data>/avatar64/.
ALTER TABLE users ADD COLUMN avatar_sha BYTEA CHECK (length(avatar_sha) = 32);
