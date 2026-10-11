-- Swarm initial schema. WAL for multi-reader durability; foreign keys on.
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    objective TEXT NOT NULL,
    status TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT,
    root_task_id TEXT,
    agent_target INTEGER NOT NULL,
    zero_cost INTEGER NOT NULL,
    budget_json TEXT NOT NULL,
    mode TEXT NOT NULL DEFAULT 'general',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    parent_id TEXT REFERENCES tasks(id) ON DELETE CASCADE,
    depth INTEGER NOT NULL,
    title TEXT NOT NULL,
    objective TEXT NOT NULL,
    role TEXT NOT NULL,
    group_name TEXT NOT NULL,
    priority INTEGER NOT NULL,
    depends_on TEXT NOT NULL,
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    lease_token TEXT,
    lease_expires_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_tasks_run_state ON tasks(run_id, state);
CREATE INDEX idx_tasks_run_group ON tasks(run_id, group_name, priority);
CREATE INDEX idx_tasks_lease ON tasks(run_id, lease_expires_at);

CREATE TABLE agents (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    parent_id TEXT,
    role TEXT NOT NULL,
    depth INTEGER NOT NULL,
    state TEXT NOT NULL,
    steps INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_agents_run ON agents(run_id, state);

CREATE TABLE agent_checkpoints (
    agent_id TEXT PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    step INTEGER NOT NULL,
    steps_taken INTEGER NOT NULL,
    conversation_len INTEGER NOT NULL,
    checkpoint_json TEXT NOT NULL,
    context_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    agent_id TEXT,
    task_id TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_events_run_seq ON events(run_id, seq);

CREATE TABLE artifacts (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    path TEXT,
    size_bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    description TEXT,
    ref_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    confidence REAL NOT NULL,
    tags TEXT NOT NULL,
    json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_findings_run ON findings(run_id, created_at);

CREATE VIRTUAL TABLE findings_fts USING fts5(content, content='findings', content_rowid='id');
CREATE TRIGGER findings_ai AFTER INSERT ON findings BEGIN
    INSERT INTO findings_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER findings_ad AFTER DELETE ON findings BEGIN
    INSERT INTO findings_fts(findings_fts, rowid, content) VALUES('delete', old.id, old.content);
END;
CREATE TRIGGER findings_au AFTER UPDATE ON findings BEGIN
    INSERT INTO findings_fts(findings_fts, rowid, content) VALUES('delete', old.id, old.content);
    INSERT INTO findings_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TABLE hypotheses (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE usage (
    run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
    json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE key_values (
    namespace TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (namespace, key)
);
