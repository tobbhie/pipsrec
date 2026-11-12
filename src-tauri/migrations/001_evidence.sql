PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS schema_versions(version INTEGER PRIMARY KEY);
INSERT OR IGNORE INTO schema_versions VALUES(1);
CREATE TABLE IF NOT EXISTS sessions(
 id TEXT PRIMARY KEY, started_at TEXT NOT NULL, ended_at TEXT,
 state TEXT NOT NULL CHECK(state IN ('recording','paused','stopped','interrupted')),
 consent_json TEXT NOT NULL CHECK(json_valid(consent_json))
);
CREATE TABLE IF NOT EXISTS session_events(
 id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
 offset_ms INTEGER NOT NULL CHECK(offset_ms>=0), captured_at TEXT NOT NULL,
 recorded_at TEXT NOT NULL, kind TEXT NOT NULL, payload_json TEXT NOT NULL CHECK(json_valid(payload_json)), media_id TEXT
);
CREATE INDEX IF NOT EXISTS events_session_time ON session_events(session_id,offset_ms);
CREATE TABLE IF NOT EXISTS media(
 id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id), event_id TEXT NOT NULL REFERENCES session_events(id),
 path TEXT NOT NULL UNIQUE, sha256 TEXT NOT NULL CHECK(length(sha256)=64), mime_type TEXT NOT NULL,size_bytes INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS jobs(
 id TEXT PRIMARY KEY, media_id TEXT NOT NULL REFERENCES media(id),kind TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('pending','processing','completed','failed','needs_review')),
 attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS recovery_files(path TEXT PRIMARY KEY,discovered_at TEXT NOT NULL,reason TEXT NOT NULL);
CREATE TRIGGER IF NOT EXISTS immutable_event_update BEFORE UPDATE ON session_events BEGIN SELECT RAISE(ABORT,'Evidence events are append-only'); END;
CREATE TRIGGER IF NOT EXISTS immutable_event_delete BEFORE DELETE ON session_events BEGIN SELECT RAISE(ABORT,'Evidence events are append-only'); END;
CREATE TRIGGER IF NOT EXISTS immutable_media_update BEFORE UPDATE ON media BEGIN SELECT RAISE(ABORT,'Media evidence is immutable'); END;
CREATE TRIGGER IF NOT EXISTS immutable_media_delete BEFORE DELETE ON media BEGIN SELECT RAISE(ABORT,'Media evidence is immutable'); END;
