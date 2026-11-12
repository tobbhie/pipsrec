#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod capture;
mod store;
use base64::Engine;
use capture::WindowInfo;
use serde::Serialize;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use store::{Result, Store};
use tauri::Manager;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

struct Active {
    id: String,
    start: Instant,
    state: String,
    windows: Vec<WindowInfo>,
    epoch: u64,
}
struct Core {
    store: Store,
    active: Option<Active>,
    last_error: Option<String>,
}
type Shared = Arc<Mutex<Core>>;
#[derive(Serialize)]
struct Status {
    active_id: Option<String>,
    state: String,
    elapsed_ms: u64,
    data_path: String,
    recorder_error: Option<String>,
    recovery_file_count: i64,
}
fn status_inner(c: &Core) -> Status {
    Status {
        active_id: c.active.as_ref().map(|a| a.id.clone()),
        state: c
            .active
            .as_ref()
            .map(|a| a.state.clone())
            .unwrap_or("idle".into()),
        elapsed_ms: c
            .active
            .as_ref()
            .map(|a| a.start.elapsed().as_millis() as u64)
            .unwrap_or(0),
        data_path: c.store.root.display().to_string(),
        recorder_error: c.last_error.clone(),
        recovery_file_count: c
            .store
            .db
            .query_row("SELECT count(*) FROM recovery_files", [], |r| r.get(0))
            .unwrap_or(0),
    }
}
#[tauri::command]
fn status(core: tauri::State<Shared>) -> Result<Status> {
    let c = core.lock().map_err(|e| e.to_string())?;
    Ok(status_inner(&c))
}
#[tauri::command]
fn list_windows() -> Result<Vec<WindowInfo>> {
    capture::windows()
}
#[tauri::command]
fn sessions(core: tauri::State<Shared>) -> Result<Vec<store::Session>> {
    core.lock().map_err(|e| e.to_string())?.store.sessions()
}
#[tauri::command]
fn events(core: tauri::State<Shared>, session_id: String) -> Result<Vec<store::Event>> {
    core.lock()
        .map_err(|e| e.to_string())?
        .store
        .events(&session_id)
}
#[tauri::command]
fn media(core: tauri::State<Shared>, media_id: String) -> Result<String> {
    let (bytes, mime) = core
        .lock()
        .map_err(|e| e.to_string())?
        .store
        .media(&media_id)?;
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}
#[tauri::command]
fn start_session(
    core: tauri::State<Shared>,
    windows: Vec<WindowInfo>,
    timezone: String,
) -> Result<String> {
    if windows.is_empty() {
        return Err("Select at least one application window.".into());
    }
    let mut c = core.lock().map_err(|e| e.to_string())?;
    if c.active.is_some() {
        return Err("A session is already active.".into());
    }
    let available = capture::windows()?;
    let approved: Vec<WindowInfo> = windows
        .iter()
        .map(|w| {
            available
                .iter()
                .find(|v| v.id == w.id && v.pid == w.pid)
                .cloned()
                .ok_or_else(|| "A selected window is no longer available.".to_string())
        })
        .collect::<Result<_>>()?;
    let sid=c.store.start(&json!({"windows":approved,"timezone":timezone,"microphone":false,"cloud_processing":false,"snapshot_interval_seconds":15,"schema_version":1}))?;
    c.active = Some(Active {
        id: sid.clone(),
        start: Instant::now(),
        state: "recording".into(),
        windows: approved,
        epoch: 0,
    });
    c.last_error = None;
    Ok(sid)
}
fn change(core: &Shared, state: &str) -> Result<()> {
    let mut c = core.lock().map_err(|e| e.to_string())?;
    let a = c.active.as_ref().ok_or("No active session.")?;
    if !matches!(
        (a.state.as_str(), state),
        ("recording", "paused")
            | ("paused", "recording")
            | ("recording", "stopped")
            | ("paused", "stopped")
    ) {
        return Err("Invalid session transition.".into());
    }
    let sid = a.id.clone();
    let offset = a.start.elapsed().as_millis() as u64;
    c.store.transition(&sid, offset, state)?;
    if state == "stopped" {
        c.active = None;
    } else {
        c.active.as_mut().unwrap().state = state.into();
        c.active.as_mut().unwrap().epoch += 1;
    }
    Ok(())
}
#[tauri::command]
fn transition(core: tauri::State<Shared>, state: String) -> Result<()> {
    change(core.inner(), &state)
}
#[tauri::command]
fn add_note(core: tauri::State<Shared>, text: String) -> Result<()> {
    let text = text.trim();
    if text.is_empty() || text.len() > 20000 {
        return Err("A note must contain 1–20,000 bytes.".into());
    }
    let c = core.lock().map_err(|e| e.to_string())?;
    let a = c
        .active
        .as_ref()
        .ok_or("Start a session before adding a note.")?;
    c.store.event(
        &a.id,
        a.start.elapsed().as_millis() as u64,
        &store::now(),
        "user_note",
        json!({"text":text,"provenance":"user_stated"}),
    )
}
fn take_snapshots(core: &Shared) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::AcqRel) {
        return Err("A snapshot batch is already in progress.".into());
    }
    struct Permit;
    impl Drop for Permit {
        fn drop(&mut self) {
            BUSY.store(false, Ordering::Release);
        }
    }
    let _permit = Permit;
    let (sid, windows, epoch) = {
        let c = core.lock().map_err(|e| e.to_string())?;
        let a = c.active.as_ref().ok_or("No active session.")?;
        if a.state != "recording" {
            return Err("Capture is paused.".into());
        }
        (a.id.clone(), a.windows.clone(), a.epoch)
    };
    for w in windows {
        let (at, offset) = {
            let c = core.lock().map_err(|e| e.to_string())?;
            match c.active.as_ref() {
                Some(a) if a.id == sid && a.state == "recording" && a.epoch == epoch => {
                    (store::now(), a.start.elapsed().as_millis() as u64)
                }
                _ => return Ok(()),
            }
        };
        let result = capture::snapshot(&w);
        let mut c = core.lock().map_err(|e| e.to_string())?;
        // Discard in-flight frames if pause/stop was pressed while capture was running.
        if !c
            .active
            .as_ref()
            .is_some_and(|a| a.id == sid && a.state == "recording" && a.epoch == epoch)
        {
            continue;
        }
        match result {
            Ok(frame) => {
                let offset = frame
                    .instant
                    .duration_since(c.active.as_ref().unwrap().start)
                    .as_millis() as u64;
                c.store.save_media(
                    &sid,
                    offset,
                    &frame.captured_at,
                    &frame.bytes,
                    "screenshot",
                    "png",
                    "image/png",
                    json!(w),
                )?;
            }
            Err(message) => c.store.event(
                &sid,
                offset,
                &at,
                "capture_error",
                json!({"window":w,"message":message}),
            )?,
        }
    }
    Ok(())
}
#[tauri::command]
async fn capture_now(core: tauri::State<'_, Shared>) -> Result<()> {
    let core = core.inner().clone();
    tauri::async_runtime::spawn_blocking(move || take_snapshots(&core))
        .await
        .map_err(|e| e.to_string())?
}
fn main() {
    let shortcut = tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, _, event| {
            if event.state == ShortcutState::Pressed {
                let core = app.state::<Shared>();
                let _ = change(core.inner(), "stopped");
            }
        })
        .build();
    let app = tauri::Builder::default()
        .plugin(shortcut)
        .setup(|app| {
            let root = if let Some(p) = std::env::var_os("TRADING_MEMORY_DATA_DIR") {
                std::path::PathBuf::from(p)
            } else {
                app.path().app_local_data_dir()?
            };
            let store = Store::open(root).map_err(std::io::Error::other)?;
            let core: Shared = Arc::new(Mutex::new(Core {
                store,
                active: None,
                last_error: None,
            }));
            app.manage(core.clone());
            app.global_shortcut().register("Ctrl+Shift+F12")?;
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(15));
                let recording = core
                    .lock()
                    .map(|c| c.active.as_ref().is_some_and(|a| a.state == "recording"))
                    .unwrap_or(false);
                if recording {
                    if let Err(error) = take_snapshots(&core) {
                        if let Ok(mut c) = core.lock() {
                            c.last_error = Some(error);
                        }
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            status,
            list_windows,
            sessions,
            events,
            media,
            start_session,
            transition,
            add_note,
            capture_now
        ])
        .build(tauri::generate_context!())
        .expect("Failed to initialize pipsrec");
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            let core = app.state::<Shared>();
            let _ = change(core.inner(), "stopped");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pause_resume_invalidates_inflight_capture_and_stop_clears_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().into()).unwrap();
        let sid = store.start(&json!({"windows":[]})).unwrap();
        let core = Arc::new(Mutex::new(Core {
            store,
            active: Some(Active {
                id: sid.clone(),
                start: Instant::now(),
                state: "recording".into(),
                windows: vec![],
                epoch: 0,
            }),
            last_error: None,
        }));
        assert!(change(&core, "recording").is_err());
        change(&core, "paused").unwrap();
        assert!(take_snapshots(&core).is_err());
        change(&core, "recording").unwrap();
        assert_eq!(core.lock().unwrap().active.as_ref().unwrap().epoch, 2);
        change(&core, "stopped").unwrap();
        assert!(core.lock().unwrap().active.is_none());
        assert!(take_snapshots(&core).is_err());
        let c = core.lock().unwrap();
        assert_eq!(c.store.sessions().unwrap()[0].state, "stopped");
        assert_eq!(c.store.events(&sid).unwrap().len(), 4);
    }
}
