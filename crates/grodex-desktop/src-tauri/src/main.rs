//! Grodex desktop (Tauri) — a GUI frontend that drives the `grodex serve`
//! agent process over ACP stdio and renders its event stream in React.
//!
//! Replaces `grodex-tui` as the interactive surface while reusing the exact
//! same agent protocol and runtime.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod sessions;
mod transport;

use std::sync::{mpsc, Mutex};

use commands::TransportState;
use tauri::{Manager, RunEvent};

fn main() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            let (tx, rx) = mpsc::channel::<transport::ControlMsg>();
            let grodex_bin = transport::resolve_grodex_bin();

            std::thread::Builder::new()
                .name("grodex-acp-worker".into())
                .spawn(move || transport::run_agent_worker(rx, handle, grodex_bin))
                .expect("failed to spawn agent worker thread");

            app.manage(TransportState(Mutex::new(tx)));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::send_command,
            commands::new_session,
            commands::ensure_agent,
            commands::list_sessions,
            commands::get_config,
            commands::delete_session,
        ])
        .build(tauri::generate_context!())
        .expect("error while building grodex desktop");

    app.run(|app_handle, event| {
        // On exit, tell the worker to stop so the spawned `grodex serve`
        // child is shut down gracefully (not orphaned).
        if matches!(event, RunEvent::Exit) {
            if let Some(state) = app_handle.try_state::<TransportState>() {
                if let Ok(tx) = state.0.lock() {
                    let _ = tx.send(transport::ControlMsg::Shutdown);
                }
            }
        }
    });
}
