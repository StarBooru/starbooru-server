-- The home page, off until an admin turns it on. NULL is the default: the
-- server's name as the title, no line under it, the most popular posts
-- rated safe, and 36 of them.
ALTER TABLE server_settings ADD COLUMN home_enabled BOOLEAN;
ALTER TABLE server_settings ADD COLUMN home_title TEXT;
ALTER TABLE server_settings ADD COLUMN home_subtitle TEXT;
ALTER TABLE server_settings ADD COLUMN home_query TEXT;
ALTER TABLE server_settings ADD COLUMN home_posts INTEGER CHECK (home_posts > 0);
