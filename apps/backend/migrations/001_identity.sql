PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS users (
    subject TEXT PRIMARY KEY NOT NULL
);
CREATE TABLE IF NOT EXISTS enrollments (
    id TEXT PRIMARY KEY NOT NULL,
    device_id TEXT UNIQUE NOT NULL,
    name TEXT NOT NULL,
    public_key TEXT NOT NULL,
    code_hash TEXT NOT NULL,
    verifier_hash TEXT NOT NULL,
    challenge TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    owner TEXT REFERENCES users(subject),
    consumed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS oauth_transactions (
    state_hash TEXT PRIMARY KEY NOT NULL,
    enrollment_id TEXT NOT NULL REFERENCES enrollments(id),
    nonce TEXT NOT NULL,
    pkce TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS confirmations (
    token_hash TEXT PRIMARY KEY NOT NULL,
    enrollment_id TEXT NOT NULL REFERENCES enrollments(id),
    subject TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS devices (
    id TEXT PRIMARY KEY NOT NULL,
    owner TEXT NOT NULL REFERENCES users(subject),
    name TEXT NOT NULL,
    public_key TEXT UNIQUE NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS device_challenges (
    id TEXT PRIMARY KEY NOT NULL,
    device_id TEXT NOT NULL REFERENCES devices(id),
    challenge TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    consumed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS access_tokens (
    token_hash TEXT PRIMARY KEY NOT NULL,
    device_id TEXT NOT NULL REFERENCES devices(id),
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS devices_owner ON devices(owner);
PRAGMA user_version = 1;
