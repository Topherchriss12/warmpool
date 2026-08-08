CREATE TABLE users (
    id UUID PRIMARY KEY,
    email TEXT NOT NULL
);


CREATE TABLE posts (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id),
    title text NOT NULL
);