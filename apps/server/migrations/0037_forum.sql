-- The forum of a public server. Categories are set by admins and stay when
-- the server goes private; threads and posts don't.
CREATE TABLE forum_categories (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    position INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX forum_categories_name ON forum_categories (lower(name));

-- `post_count` and `last_post_at` move with each post, so lists don't count.
CREATE TABLE forum_threads (
    id BIGSERIAL PRIMARY KEY,
    category_id BIGINT NOT NULL REFERENCES forum_categories(id),
    title TEXT NOT NULL,
    author_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_post_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    post_count INTEGER NOT NULL DEFAULT 0,
    pinned BOOLEAN NOT NULL DEFAULT false,
    locked BOOLEAN NOT NULL DEFAULT false
);
-- A category's list: pinned first, then the latest activity.
CREATE INDEX forum_threads_list ON forum_threads (category_id, pinned DESC, last_post_at DESC, id DESC);
CREATE INDEX forum_threads_recent ON forum_threads (last_post_at DESC, id DESC);
CREATE INDEX forum_threads_title ON forum_threads USING gin (to_tsvector('simple', title));

-- A deleted post keeps its place with an empty body.
CREATE TABLE forum_posts (
    id BIGSERIAL PRIMARY KEY,
    thread_id BIGINT NOT NULL REFERENCES forum_threads(id) ON DELETE CASCADE,
    author_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    body TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    edited_at TIMESTAMPTZ,
    deleted_at TIMESTAMPTZ
);
CREATE INDEX forum_posts_thread ON forum_posts (thread_id, id);
-- The posting limits count an account's recent posts.
CREATE INDEX forum_posts_author ON forum_posts (author_id, created_at);
CREATE INDEX forum_posts_body ON forum_posts USING gin (to_tsvector('simple', body))
    WHERE deleted_at IS NULL;
