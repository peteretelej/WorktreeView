//! The user-driven updater. One flow serves both surfaces: the Settings
//! About row mirrors the `update-status` event stream, and the tray menu
//! item reports through dialogs. Checks only ever run on request, and
//! Store/MSIX installs never see the updater because the Store owns their
//! updates.

use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_updater::UpdaterExt;

// Flat shape on purpose: the Settings page matches phases by string and
// every field is optional.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpdateStatus {
    phase: &'static str,
    version: Option<String>,
    notes: Option<String>,
    received: Option<u64>,
    total: Option<u64>,
    message: Option<String>,
}

impl UpdateStatus {
    pub(crate) fn new(phase: &'static str) -> Self {
        Self { phase, version: None, notes: None, received: None, total: None, message: None }
    }

    fn version_ready(version: String) -> Self {
        Self { version: Some(version), ..Self::new("ready") }
    }

    fn failed(message: String) -> Self {
        Self { message: Some(crate::sanitize(&message)), ..Self::new("failed") }
    }
}

pub(crate) struct UpdateState(pub(crate) Mutex<UpdateStatus>);
pub(crate) struct PendingUpdate(pub(crate) Mutex<Option<tauri_plugin_updater::Update>>);

fn set_phase(app: &AppHandle, phase: UpdateStatus) {
    if let Some(state) = app.try_state::<UpdateState>() {
        *state.0.lock().expect("update state mutex poisoned") = phase.clone();
    }
    let _ = app.emit("update-status", phase);
}

// MSIX/Store installs take their updates from the Store; the packaged
// environment exposes the package family name to the app (and to any
// process it spawns), so the value must name this app and not merely
// exist.
fn store_managed() -> bool {
    std::env::var("PACKAGE_FAMILY_NAME")
        .map(|family| family.to_ascii_lowercase().contains("worktreeview"))
        .unwrap_or(false)
}

// One shared phase machine drives both surfaces, so checking or
// downloading anywhere blocks a second flow everywhere.
fn busy(app: &AppHandle) -> bool {
    app.try_state::<UpdateState>()
        .map(|state| matches!(state.0.lock().expect("update state mutex poisoned").phase, "checking" | "downloading"))
        .unwrap_or(false)
}

fn store_pending(app: &AppHandle, update: tauri_plugin_updater::Update) {
    if let Some(pending) = app.try_state::<PendingUpdate>() {
        *pending.0.lock().expect("pending update mutex poisoned") = Some(update);
    }
}

fn take_pending(app: &AppHandle) -> Option<tauri_plugin_updater::Update> {
    app.try_state::<PendingUpdate>()
        .and_then(|pending| pending.0.lock().expect("pending update mutex poisoned").take())
}

pub(crate) fn start_check(app: AppHandle) {
    if busy(&app) {
        return;
    }
    if store_managed() {
        set_phase(&app, UpdateStatus::new("managed_by_store"));
        return;
    }
    set_phase(&app, UpdateStatus::new("checking"));
    tauri::async_runtime::spawn(async move {
        let found = match app.updater() {
            Ok(updater) => updater.check().await,
            Err(error) => Err(error),
        };
        match found {
            Ok(Some(update)) => {
                let version = update.version.clone();
                let notes = update.body.clone();
                store_pending(&app, update);
                set_phase(&app, UpdateStatus {
                    notes,
                    version: Some(version),
                    ..UpdateStatus::new("available")
                });
            }
            Ok(None) => set_phase(&app, UpdateStatus::new("up_to_date")),
            Err(error) => set_phase(&app, UpdateStatus::failed(error.to_string())),
        }
    });
}

pub(crate) fn start_install(app: AppHandle) {
    if busy(&app) {
        return;
    }
    let Some(update) = app.state::<PendingUpdate>().0.lock().expect("pending update mutex poisoned").take() else {
        return;
    };
    set_phase(&app, UpdateStatus::new("downloading"));
    let version = update.version.clone();
    let progress_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut received: u64 = 0;
        let result = update
            .download_and_install(
                move |chunk, total| {
                    received += chunk as u64;
                    set_phase(&progress_app, UpdateStatus {
                        received: Some(received),
                        total,
                        ..UpdateStatus::new("downloading")
                    });
                },
                || {},
            )
            .await;
        match result {
            // Windows never reaches this arm: the installer takes the
            // process down and relaunches the app itself.
            Ok(()) => set_phase(&app, UpdateStatus::version_ready(version)),
            Err(error) => set_phase(&app, UpdateStatus::failed(error.to_string())),
        }
    });
}

// The tray path reports through dialogs instead of the Settings event
// stream, so one blocking task runs the flow to completion.
pub(crate) fn check_from_tray(app: AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        tauri::async_runtime::block_on(tray_update_flow(app));
    });
}

async fn tray_update_flow(app: AppHandle) {
    if busy(&app) {
        app.dialog().message("An update check or install is already in progress.").title("WorktreeView").blocking_show();
        return;
    }
    if store_managed() {
        app.dialog().message("Updates for WorktreeView are managed by the Microsoft Store.").title("WorktreeView").blocking_show();
        return;
    }
    set_phase(&app, UpdateStatus::new("checking"));
    let found = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(error) => Err(error),
    };
    match found {
        Ok(None) => {
            set_phase(&app, UpdateStatus::new("up_to_date"));
            app.dialog().message("WorktreeView is up to date.").title("WorktreeView").blocking_show();
        }
        Ok(Some(update)) => {
            // Shared with the Settings surface: whichever acts first takes
            // the pending update, and the other one stands down.
            let version = update.version.clone();
            let notes = update.body.clone();
            store_pending(&app, update);
            set_phase(&app, UpdateStatus {
                notes,
                version: Some(version.clone()),
                ..UpdateStatus::new("available")
            });
            let install = app.dialog()
                .message(format!("WorktreeView {version} is available. Download and install it now?"))
                .title("WorktreeView")
                .buttons(MessageDialogButtons::OkCancelCustom("Install".into(), "Later".into()))
                .blocking_show();
            let Some(update) = take_pending(&app) else { return; };
            if !install {
                set_phase(&app, UpdateStatus::new("idle"));
                return;
            }
            set_phase(&app, UpdateStatus::new("downloading"));
            let mut received: u64 = 0;
            let progress_app = app.clone();
            let installed = update
                .download_and_install(
                    move |chunk, total| {
                        received += chunk as u64;
                        set_phase(&progress_app, UpdateStatus {
                            received: Some(received),
                            total,
                            ..UpdateStatus::new("downloading")
                        });
                    },
                    || {},
                )
                .await;
            match installed {
                // Windows never reaches this arm: the installer takes the
                // process down and relaunches the app itself.
                Ok(()) => {
                    set_phase(&app, UpdateStatus::version_ready(version.clone()));
                    let restart = app.dialog()
                        .message(format!("WorktreeView {version} is installed. Restart to finish updating?"))
                        .title("WorktreeView")
                        .buttons(MessageDialogButtons::OkCancelCustom("Restart".into(), "Later".into()))
                        .blocking_show();
                    if restart {
                        let _ = app.restart();
                    }
                }
                Err(error) => {
                    set_phase(&app, UpdateStatus::failed(error.to_string()));
                    app.dialog().message(format!("The update could not be installed: {error}")).title("WorktreeView").blocking_show();
                }
            }
        }
        Err(error) => {
            set_phase(&app, UpdateStatus::failed(error.to_string()));
            app.dialog().message(format!("The update check failed: {error}")).title("WorktreeView").blocking_show();
        }
    }
}

#[tauri::command]
pub(crate) fn get_update_status(state: tauri::State<'_, UpdateState>) -> UpdateStatus {
    state.0.lock().expect("update state mutex poisoned").clone()
}

#[tauri::command]
pub(crate) fn check_for_updates(app: AppHandle) {
    start_check(app);
}

#[tauri::command]
pub(crate) fn install_update(app: AppHandle) {
    start_install(app);
}

#[tauri::command]
pub(crate) fn restart_app(app: AppHandle) {
    let _ = app.restart();
}
