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

-- Audit log table
CREATE TABLE IF NOT EXISTS audit_log (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID REFERENCES users(id),
    action TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Trigger function
CREATE OR REPLACE FUNCTION log_user_action()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO audit_log (user_id, action)
    VALUES (NEW.id, 'insert');
    RETURN NEW;
END;
$$;

-- Trigger on users
DROP TRIGGER IF EXISTS users_audit_insert ON users;
CREATE TRIGGER users_audit_insert
AFTER INSERT ON users
FOR EACH ROW
EXECUTE FUNCTION log_user_action();

COMMIT;