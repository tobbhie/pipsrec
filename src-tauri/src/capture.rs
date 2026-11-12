use crate::store::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct WindowInfo {
    pub id: usize,
    pub pid: u32,
    pub title: String,
    pub process: String,
}
pub struct CapturedFrame {
    pub bytes: Vec<u8>,
    pub captured_at: String,
    pub instant: std::time::Instant,
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{sync::mpsc, time::Duration};
    use windows_capture::{
        capture::{Context, GraphicsCaptureApiHandler},
        encoder::ImageFormat,
        frame::Frame,
        graphics_capture_api::InternalCaptureControl,
        settings::{
            ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
            MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
        },
        window::Window,
    };

    pub fn windows() -> Result<Vec<WindowInfo>> {
        Ok(Window::enumerate()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|w| {
                let title = w.title().ok()?;
                let pid = w.process_id().ok()?;
                if title.is_empty() || pid == std::process::id() {
                    return None;
                }
                Some(WindowInfo {
                    id: w.as_raw_hwnd() as usize,
                    pid,
                    title,
                    process: w
                        .process_name()
                        .unwrap_or_else(|_| "Unknown application".into()),
                })
            })
            .collect())
    }
    struct Shot {
        tx: mpsc::SyncSender<Result<CapturedFrame>>,
    }
    impl GraphicsCaptureApiHandler for Shot {
        type Flags = mpsc::SyncSender<Result<CapturedFrame>>;
        type Error = Box<dyn std::error::Error + Send + Sync>;
        fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
            Ok(Self { tx: ctx.flags })
        }
        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            control: InternalCaptureControl,
        ) -> std::result::Result<(), Self::Error> {
            let captured_at = crate::store::now();
            let instant = std::time::Instant::now();
            let path =
                std::env::temp_dir().join(format!("trading-memory-{}.png", crate::store::id()));
            let result = frame
                .save_as_image(&path, ImageFormat::Png)
                .map_err(|e| e.to_string())
                .and_then(|_| std::fs::read(&path).map_err(|e| e.to_string()))
                .map(|bytes| CapturedFrame {
                    bytes,
                    captured_at,
                    instant,
                });
            let _ = std::fs::remove_file(path);
            let _ = self.tx.try_send(result);
            control.stop();
            Ok(())
        }
        fn on_closed(&mut self) -> std::result::Result<(), Self::Error> {
            let _ = self
                .tx
                .try_send(Err("Approved window closed during capture".into()));
            Ok(())
        }
    }
    pub fn snapshot(approved: &WindowInfo) -> Result<CapturedFrame> {
        // Re-enumerate by handle AND process id. Never retarget by title or capture a monitor.
        if !windows()?
            .iter()
            .any(|w| w.id == approved.id && w.pid == approved.pid)
        {
            return Err("Approved window is unavailable. Select it again after stopping.".into());
        }
        let window = Window::from_raw_hwnd(approved.id as *mut std::ffi::c_void);
        let (tx, rx) = mpsc::sync_channel(1);
        let settings = Settings::new(
            window,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Exclude,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba8,
            tx,
        );
        let control = Shot::start_free_threaded(settings).map_err(|e| e.to_string())?;
        let result=rx.recv_timeout(Duration::from_secs(5)).map_err(|_|"No frame arrived within 5 seconds; the window may be minimized or capture may be blocked.".to_string()).and_then(|v|v);
        let stopped = control.stop().map_err(|e| e.to_string());
        result.and_then(|frame| stopped.map(|_| frame))
    }
}
#[cfg(windows)]
pub use native::{snapshot, windows};
#[cfg(not(windows))]
pub fn windows() -> Result<Vec<WindowInfo>> {
    Err("Native capture is implemented for the Windows spike only.".into())
}
#[cfg(not(windows))]
pub fn snapshot(_: &WindowInfo) -> Result<CapturedFrame> {
    Err("Capture is unavailable on this platform.".into())
}
