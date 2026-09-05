BEGIN;

-- Provide gen_random_uuid()
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- Create users table
CREATE TABLE  IF NOT EXISTS users (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    username TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    is_admin BOOLEAN NOT NULL DEFAULT false,
    is_disabled BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Create tags table
CREATE TABLE IF NOT EXISTS tags (
    id serial PRIMARY KEY,
    name text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Create content_items table
CREATE TABLE IF NOT EXISTS content_items (
    id serial PRIMARY KEY,
    slug text NOT NULL UNIQUE,
    title text NOT NULL,
    body text NOT NULL,
    kind text NOT NULL,
    read_time_min integer,
    published_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Join table content_items and tags
CREATE TABLE IF NOT EXISTS content_item_tags (
    content_item_id integer NOT NULL REFERENCES content_items(id) ON DELETE CASCADE,
    tag_id integer NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (content_item_id, tag_id)
);

-- indexes
CREATE INDEX IF NOT EXISTS idx_content_items_published_at ON content_items (published_at);
CREATE INDEX IF NOT EXISTS idx_tags_name ON tags (name);

COMMIT;