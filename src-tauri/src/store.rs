use chrono::Utc;
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, String>;
pub fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn id() -> String {
    Uuid::new_v4().to_string()
}

pub struct Store {
    pub db: Connection,
    pub root: PathBuf,
}

#[derive(Serialize)]
pub struct Session {
    pub id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub state: String,
    pub consent: Value,
}
#[derive(Serialize)]
pub struct Event {
    pub id: String,
    pub session_id: String,
    pub offset_ms: i64,
    pub captured_at: String,
    pub recorded_at: String,
    pub kind: String,
    pub payload: Value,
    pub media_id: Option<String>,
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("media")).map_err(|e| e.to_string())?;
        let db = Connection::open(root.join("trading-memory.sqlite")).map_err(|e| e.to_string())?;
        db.execute_batch(include_str!("../migrations/001_evidence.sql"))
            .map_err(|e| e.to_string())?;
        let mut s = Self { db, root };
        // Never resume capture automatically after an interrupted process.
        let interrupted: Vec<String> = {
            let mut q =
                s.db.prepare("SELECT id FROM sessions WHERE state IN ('recording','paused')")
                    .map_err(|e| e.to_string())?;
            let rows = q.query_map([], |r| r.get(0)).map_err(|e| e.to_string())?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(|e| e.to_string())?
        };
        for sid in interrupted {
            s.event(&sid, 0, &now(), "recovery", json!({"message":"Previous process interrupted. Recording remains stopped; last event timestamps are preserved.","offset_unknown":true}))?;
            s.db.execute(
                "UPDATE sessions SET state='interrupted', ended_at=? WHERE id=?",
                params![now(), sid],
            )
            .map_err(|e| e.to_string())?;
        }
        s.db.execute("UPDATE jobs SET state='pending',last_error='Interrupted processing; queued again' WHERE state='processing'",[]).map_err(|e|e.to_string())?;
        s.reconcile_files()?;
        Ok(s)
    }
    fn reconcile_files(&mut self) -> Result<()> {
        // Files flushed before a failed DB transaction are retained and surfaced for review.
        // Incomplete .pending files are preserved as recovery evidence, never interpreted as images.
        let entries = fs::read_dir(self.root.join("media")).map_err(|e| e.to_string())?;
        for dir in entries {
            let dir = dir.map_err(|e| e.to_string())?;
            if !dir.file_type().map_err(|e| e.to_string())?.is_dir() {
                continue;
            }
            for file in fs::read_dir(dir.path()).map_err(|e| e.to_string())? {
                let file = file.map_err(|e| e.to_string())?;
                if !file.file_type().map_err(|e| e.to_string())?.is_file() {
                    continue;
                }
                let rel = file
                    .path()
                    .strip_prefix(&self.root)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                let linked: bool = self
                    .db
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM media WHERE path=?)",
                        [&rel],
                        |r| r.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                if !linked {
                    self.db.execute("INSERT OR IGNORE INTO recovery_files(path,discovered_at,reason) VALUES(?,?,'Unlinked or incomplete evidence; preserved for manual recovery')",params![rel,now()]).map_err(|e|e.to_string())?;
                }
            }
        }
        Ok(())
    }
    pub fn start(&mut self, consent: &Value) -> Result<String> {
        let sid = id();
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO sessions(id,started_at,state,consent_json) VALUES(?,?,'recording',?)",
            params![sid, now(), consent.to_string()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO session_events(id,session_id,offset_ms,captured_at,recorded_at,kind,payload_json) VALUES(?,?,0,?,?,'session_started',?)",params![id(),sid,now(),now(),consent.to_string()]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(sid)
    }
    pub fn event(
        &self,
        sid: &str,
        offset: u64,
        at: &str,
        kind: &str,
        payload: Value,
    ) -> Result<()> {
        self.db.execute("INSERT INTO session_events(id,session_id,offset_ms,captured_at,recorded_at,kind,payload_json) VALUES(?,?,?,?,?,?,?)",params![id(),sid,offset as i64,at,now(),kind,payload.to_string()]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn transition(&mut self, sid: &str, offset: u64, state: &str) -> Result<()> {
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        tx.execute("UPDATE sessions SET state=?, ended_at=CASE WHEN ?='stopped' THEN ? ELSE ended_at END WHERE id=?",params![state,state,now(),sid]).map_err(|e|e.to_string())?;
        tx.execute("INSERT INTO session_events(id,session_id,offset_ms,captured_at,recorded_at,kind,payload_json) VALUES(?,?,?,?,?,'session_state',?)",params![id(),sid,offset as i64,now(),now(),json!({"state":state}).to_string()]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn save_media(
        &mut self,
        sid: &str,
        offset: u64,
        at: &str,
        bytes: &[u8],
        kind: &str,
        ext: &str,
        mime: &str,
        source: Value,
    ) -> Result<String> {
        let mid = id();
        let eid = id();
        let rel = format!("media/{sid}/{mid}.{ext}");
        let path = self.root.join(&rel);
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let pending = path.with_extension(format!("{ext}.pending"));
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)
            .map_err(|e| e.to_string())?;
        f.write_all(bytes)
            .and_then(|_| f.sync_all())
            .map_err(|e| e.to_string())?;
        drop(f);
        fs::rename(&pending, &path).map_err(|e| e.to_string())?;
        let hash = format!("{:x}", Sha256::digest(bytes));
        let payload = json!({"source":source,"sha256":hash,"size_bytes":bytes.len(),"privacy":"local_only_unredacted"});
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO session_events(id,session_id,offset_ms,captured_at,recorded_at,kind,payload_json,media_id) VALUES(?,?,?,?,?,?,?,?)",params![eid,sid,offset as i64,at,now(),kind,payload.to_string(),mid]).map_err(|e|e.to_string())?;
        tx.execute("INSERT INTO media(id,session_id,event_id,path,sha256,mime_type,size_bytes) VALUES(?,?,?,?,?,?,?)",params![mid,sid,eid,rel,hash,mime,bytes.len() as i64]).map_err(|e|e.to_string())?;
        tx.execute(
            "INSERT INTO jobs(id,media_id,kind,state,created_at) VALUES(?,?,?,'pending',?)",
            params![
                id(),
                mid,
                if kind == "audio" {
                    "transcription"
                } else {
                    "vision"
                },
                now()
            ],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(mid)
    }
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut q=self.db.prepare("SELECT id,started_at,ended_at,state,consent_json FROM sessions ORDER BY started_at DESC").map_err(|e|e.to_string())?;
        let rows = q
            .query_map([], |r| {
                let raw: String = r.get(4)?;
                Ok(Session {
                    id: r.get(0)?,
                    started_at: r.get(1)?,
                    ended_at: r.get(2)?,
                    state: r.get(3)?,
                    consent: serde_json::from_str(&raw).unwrap_or(Value::Null),
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(|e| e.to_string())
    }
    pub fn events(&self, sid: &str) -> Result<Vec<Event>> {
        let mut q=self.db.prepare("SELECT id,session_id,offset_ms,captured_at,recorded_at,kind,payload_json,media_id FROM session_events WHERE session_id=? ORDER BY offset_ms,rowid").map_err(|e|e.to_string())?;
        let rows = q
            .query_map([sid], |r| {
                let raw: String = r.get(6)?;
                Ok(Event {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    offset_ms: r.get(2)?,
                    captured_at: r.get(3)?,
                    recorded_at: r.get(4)?,
                    kind: r.get(5)?,
                    payload: serde_json::from_str(&raw).unwrap_or(Value::Null),
                    media_id: r.get(7)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(|e| e.to_string())
    }
    pub fn media(&self, mid: &str) -> Result<(Vec<u8>, String)> {
        let (rel, mime): (String, String) = self
            .db
            .query_row("SELECT path,mime_type FROM media WHERE id=?", [mid], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|e| e.to_string())?;
        Ok((
            fs::read(self.root.join(rel)).map_err(|e| e.to_string())?,
            mime,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unlinked_files_are_preserved_and_reported_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut s = Store::open(dir.path().into()).unwrap();
            let sid = s.start(&json!({})).unwrap();
            let p = dir.path().join("media").join(sid);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("unlinked.png.pending"), b"incomplete").unwrap();
        }
        let s = Store::open(dir.path().into()).unwrap();
        assert_eq!(
            s.db.query_row("SELECT count(*) FROM recovery_files", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let rel: String =
            s.db.query_row("SELECT path FROM recovery_files", [], |r| r.get(0))
                .unwrap();
        assert_eq!(fs::read(dir.path().join(rel)).unwrap(), b"incomplete");
    }
    #[test]
    fn evidence_is_hashed_linked_and_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path().into()).unwrap();
        let sid = s.start(&json!({"windows":[1]})).unwrap();
        let mid = s
            .save_media(
                &sid,
                12,
                &now(),
                b"original evidence",
                "screenshot",
                "png",
                "image/png",
                json!({"window":1}),
            )
            .unwrap();
        assert_eq!(s.media(&mid).unwrap().0, b"original evidence");
        assert_eq!(s.events(&sid).unwrap().len(), 2);
        assert!(s
            .db
            .execute("UPDATE session_events SET kind='fake'", [])
            .is_err());
        assert!(s.db.execute("DELETE FROM media", []).is_err());
        assert_eq!(
            s.db.query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn restart_interrupts_without_resuming() {
        let dir = tempfile::tempdir().unwrap();
        let sid;
        {
            let mut s = Store::open(dir.path().into()).unwrap();
            sid = s.start(&json!({})).unwrap();
            s.transition(&sid, 100, "paused").unwrap();
        }
        let s = Store::open(dir.path().into()).unwrap();
        assert_eq!(s.sessions().unwrap()[0].state, "interrupted");
        assert!(s.events(&sid).unwrap().iter().any(|e| e.kind == "recovery"));
    }
}
