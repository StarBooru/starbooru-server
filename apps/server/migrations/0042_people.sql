-- People: a franchise and a face for each figure, and the franchise each
-- character belongs to.

-- Slot 13 is the figure's franchise.
ALTER TABLE figure_tags DROP CONSTRAINT figure_tags_slot_check;
ALTER TABLE figure_tags ADD CONSTRAINT figure_tags_slot_check CHECK (slot BETWEEN 0 AND 13);

-- Where the face is, in thousandths of the picture's width and height, and
-- the kept crop of it, a file named by its SHA-256. `face_failed`: the crop
-- couldn't be made; it is tried again at the next start.
ALTER TABLE figures
    ADD COLUMN face_x SMALLINT,
    ADD COLUMN face_y SMALLINT,
    ADD COLUMN face_w SMALLINT,
    ADD COLUMN face_h SMALLINT,
    ADD COLUMN face_sha BYTEA,
    ADD COLUMN face_failed BOOLEAN NOT NULL DEFAULT false,
    ADD CONSTRAINT figures_face CHECK (
        (face_x IS NULL AND face_y IS NULL AND face_w IS NULL AND face_h IS NULL
         AND face_sha IS NULL)
        OR (face_x >= 0 AND face_y >= 0 AND face_w > 0 AND face_h > 0
            AND face_x + face_w <= 1000 AND face_y + face_h <= 1000));
CREATE INDEX figures_faces_due ON figures (post_id)
    WHERE face_x IS NOT NULL AND face_sha IS NULL AND NOT face_failed;
CREATE INDEX figures_face_sha ON figures (face_sha) WHERE face_sha IS NOT NULL;

-- A character's franchise as the server worked it out from the collection,
-- the figures with the best faces to show for it, and when; NULL until it
-- has looked.
ALTER TABLE tags
    ADD COLUMN franchise_id BIGINT REFERENCES tags (id) ON DELETE SET NULL,
    ADD COLUMN face_figures BIGINT[] NOT NULL DEFAULT '{}',
    ADD COLUMN franchise_checked_at TIMESTAMPTZ;
CREATE INDEX tags_franchise ON tags (franchise_id) WHERE franchise_id IS NOT NULL;
