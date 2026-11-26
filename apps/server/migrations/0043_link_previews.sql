-- Link previews: chats and sites show a shared address with its picture.
-- The admin's switch, NULL for off: on a public server it covers posts'
-- own addresses, on a private one it lets accounts turn previews on.
ALTER TABLE server_settings ADD COLUMN link_previews BOOLEAN;

-- What an account's links do when they don't choose.
ALTER TABLE users ADD COLUMN share_previews BOOLEAN NOT NULL DEFAULT false;

-- A link's own choice; NULL follows its owner's.
ALTER TABLE shares ADD COLUMN preview BOOLEAN;
