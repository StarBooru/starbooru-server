-- Votes on a post's tags (public servers): 1 "belongs here", -1 "doesn't".
-- A tag's score is the sum, worked out with the post.
CREATE TABLE tag_votes (
    post_id BIGINT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    tag_id BIGINT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    value SMALLINT NOT NULL CHECK (value IN (-1, 1)),
    voted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (post_id, tag_id, user_id)
);
CREATE INDEX tag_votes_user ON tag_votes (user_id);
CREATE INDEX tag_votes_tag ON tag_votes (tag_id);

-- Tags the votes took off a post. While a row is here, edits don't put the
-- tag back; an admin's undo deletes it.
CREATE TABLE tag_removals (
    id BIGSERIAL PRIMARY KEY,
    post_id BIGINT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    tag_id BIGINT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    score INTEGER NOT NULL,
    removed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (post_id, tag_id)
);
CREATE INDEX tag_removals_newest ON tag_removals (removed_at DESC, id DESC);
CREATE INDEX tag_removals_tag ON tag_removals (tag_id);

-- The score at which a tag comes off, as a positive number (3 means -3);
-- 0 never. NULL is the default, 3.
ALTER TABLE server_settings ADD COLUMN tag_vote_threshold INTEGER
    CHECK (tag_vote_threshold BETWEEN 0 AND 100);
