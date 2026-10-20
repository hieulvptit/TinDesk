CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY NOT NULL,
    owner_subject TEXT NOT NULL REFERENCES users(subject),
    client_id TEXT NOT NULL REFERENCES devices(id),
    agent_id TEXT NOT NULL REFERENCES devices(id),
    lease_expires_at INTEGER NOT NULL,
    ready INTEGER NOT NULL DEFAULT 0,
    closed INTEGER NOT NULL DEFAULT 0,
    ice_servers TEXT NOT NULL DEFAULT '[]',
    turn_expires_at INTEGER NOT NULL DEFAULT 0,
    refresh_generation INTEGER NOT NULL DEFAULT 0,
    refresh_pending INTEGER NOT NULL DEFAULT 0,
    refresh_started_at INTEGER NOT NULL DEFAULT 0,
    client_installed INTEGER NOT NULL DEFAULT 0,
    agent_installed INTEGER NOT NULL DEFAULT 0,
    CHECK(client_id != agent_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_agent ON sessions(agent_id) WHERE closed=0;
CREATE INDEX IF NOT EXISTS sessions_owner ON sessions(owner_subject);
