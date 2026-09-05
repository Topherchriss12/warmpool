BEGIN;

-- Seed data
INSERT INTO tags (name) SELECT 'tag_' || g FROM generate_series(1, 50) g;

INSERT INTO content_items (slug, title, body, kind, read_time_min, published_at)
SELECT
    'post-' || g,
    'Post title ' || g,
    repeat('lorem ipsum dolor sit amet consectetur adipiscing elit ', 200),
    CASE WHEN g % 3 = 0 THEN 'article' ELSE 'note' END,
    (g % 10) + 1,
    CURRENT_DATE - (g || ' days')::interval
FROM generate_series(1, 500) g;

INSERT INTO content_item_tags (content_item_id, tag_id)
SELECT ci.id, t.id
FROM content_items ci
JOIN tags t ON (t.id % 7) = (hashtext(ci.slug) % 7 + 7) % 7
LIMIT 2000;

INSERT INTO users (username, email, password_hash, is_admin)
VALUES ('demo_admin', 'demo_admin@example.com', 'argon2id$fake$demo$hash$for$bench$only', true)
ON CONFLICT (email) DO NOTHING;

COMMIT;