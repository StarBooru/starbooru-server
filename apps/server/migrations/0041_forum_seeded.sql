-- Whether the forum has had its default categories, so a public server gets
-- them at start once and an admin who deleted them all doesn't get them
-- back. A forum that has or ever had a category counts as seeded: one from
-- before this that got none (a server public before 0.4.0) doesn't.
ALTER TABLE server_settings ADD COLUMN forum_seeded BOOLEAN NOT NULL DEFAULT false;
UPDATE server_settings SET forum_seeded =
    EXISTS (SELECT 1 FROM forum_categories)
    OR (SELECT is_called FROM forum_categories_id_seq);
