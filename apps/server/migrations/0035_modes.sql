-- Private and public instances, and scores as likes or as stars.

-- NULL follows STARBOORU_MODE and STARBOORU_SCORES. A switch of mode writes
-- the mode out, so the variable doesn't move it afterwards.
ALTER TABLE server_settings ADD COLUMN mode TEXT CHECK (mode IN ('private', 'public'));
ALTER TABLE server_settings ADD COLUMN score_kind TEXT CHECK (score_kind IN ('likes', 'stars'));

-- A row without stars is a like (1) or a dislike (-1). A star vote keeps its
-- stars and, in `value`, the like they count as (4 or 5 a like, 1 or 2 a
-- dislike, 3 neither), so `posts.score` is the net of likes under both.
ALTER TABLE post_votes DROP CONSTRAINT post_votes_value_check;
ALTER TABLE post_votes ADD COLUMN stars SMALLINT CHECK (stars BETWEEN 1 AND 5);
ALTER TABLE post_votes ADD CONSTRAINT post_votes_value_check CHECK (
    CASE WHEN stars IS NULL THEN value IN (-1, 1)
         ELSE value = CASE WHEN stars >= 4 THEN 1 WHEN stars <= 2 THEN -1 ELSE 0 END
    END
);

-- The stars of a post's votes added up, and how many there are.
ALTER TABLE posts ADD COLUMN star_sum INTEGER NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN star_count INTEGER NOT NULL DEFAULT 0;
-- `order:score` under stars: the stars as if each post had five votes of
-- three already (`starbooru_core::mode::fair_rating`). routes/posts.rs sorts
-- by the same expression.
CREATE INDEX posts_live_stars ON posts (((star_sum + 15)::float8 / (star_count + 5)) DESC, id DESC)
    WHERE deleted_at IS NULL;
