BEGIN;

-- Provide gen_random_uuid()
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- Users table
CREATE TABLE IF NOT EXISTS users (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email TEXT NOT NULL UNIQUE,
    username TEXT,
    password_hash TEXT NOT NULL,
    is_admin BOOLEAN NOT NULL DEFAULT false,
    is_disabled BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Tags table
CREATE TABLE IF NOT EXISTS tags (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Content items table
CREATE TABLE IF NOT EXISTS content_items (
    id SERIAL PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    kind TEXT NOT NULL,
    read_time_min INTEGER,
    published_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Join table: content_items <-> tags
CREATE TABLE IF NOT EXISTS content_item_tags (
    content_item_id INTEGER NOT NULL REFERENCES content_items(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (content_item_id, tag_id)
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_content_items_published_at ON content_items (published_at);
CREATE INDEX IF NOT EXISTS idx_tags_name ON tags (name);

-- Seed tags
INSERT INTO tags (name)
SELECT 'tag_' || g
FROM generate_series(1, 50) AS g;

-- Seed content_items
INSERT INTO content_items (slug, title, body, kind, read_time_min, published_at)
SELECT
    'post-' || g,
    'Post title ' || g,
    repeat('lorem ipsum dolor sit amet consectetur adipiscing elit ', 200),
    CASE WHEN g % 3 = 0 THEN 'article' ELSE 'note' END,
    (g % 10) + 1,
    CURRENT_DATE - (g || ' days')::interval
FROM generate_series(1, 500) AS g;

-- Seed content_item_tags
INSERT INTO content_item_tags (content_item_id, tag_id)
SELECT ci.id, t.id
FROM content_items ci
JOIN tags t ON (t.id % 7) = (hashtext(ci.slug) % 7 + 7) % 7
LIMIT 2000;

-- Seed a demo admin user
INSERT INTO users (username, email, password_hash, is_admin)
VALUES ('demo_admin', 'demo_admin@example.com', 'argon2id$fake$demo$hash$for$bench$only', true);

COMMIT;