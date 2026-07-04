mod admin;
mod devices;
mod drive_mgmt;
mod firestore;
mod gdrive;
mod gdrive_auth;
mod http_client;
mod models;
mod settings;
mod sync;
mod tray;
mod watcher;

use std::sync::{Arc, Mutex};

use tauri::Manager;

use models::{
    AddGamePayload, AdminConfig, AppSettings, AuthStatus, DashboardData, DeviceInfo,
    DriveFileFlatItem, DriveFileItem, DriveVersionBackup, GoogleUserInfo, OAuthCredentials,
    PathValidation, SaveInfo, SaveTokensPayload, SyncResult, SyncStructureDiff, UpdateGamePayload,
    UserProfile, UserRole,
};

#[tauri::command]
fn load_dashboard(app: tauri::AppHandle) -> Result<DashboardData, String> {
    // Keep playtime aligned across devices (local vs Firestore) before serving UI data.
    if let Err(e) = settings::reconcile_playtime_with_firestore(&app) {
        eprintln!("[firestore] load_dashboard reconcile_playtime_with_firestore failed: {e}");
    }

    // Pull the latest cloud snapshot before reading local state so startup and manual
    // refresh both see the current library/settings on this device.
    if let Err(e) = settings::fetch_all_from_firestore(&app) {
        eprintln!("[firestore] load_dashboard fetch_all_from_firestore failed: {e}");
    }

    let mut state = settings::load_state(&app)?;
    // Merge device-specific path_overrides into save_paths (transient — not persisted).
    settings::apply_path_overrides(&mut state.games, &state.settings);
    // Populate last_local_modified dynamically by scanning all local save folders.
    for game in state.games.iter_mut() {
        let effectives = settings::effective_save_paths(game, &state.settings);
        let mut max_modified: Option<String> = None;
        for effective in &effectives {
            if let Some(ref save_path) = effective {
                let expanded = settings::expand_env_vars(save_path);
                if let Some(ts) = sync::scan_last_modified(std::path::Path::new(&expanded)) {
                    match &max_modified {
                        None => max_modified = Some(ts),
                        Some(current) if ts > *current => max_modified = Some(ts),
                        _ => {}
                    }
                }
            }
        }
        game.last_local_modified = max_modified;
    }
    Ok(DashboardData { games: state.games })
}

#[tauri::command]
fn add_manual_game(
    app: tauri::AppHandle,
    payload: AddGamePayload,
) -> Result<DashboardData, String> {
    settings::add_manual_game(&app, payload)?;
    let mut state = settings::load_state(&app)?;
    settings::apply_path_overrides(&mut state.games, &state.settings);
    Ok(DashboardData { games: state.games })
}

#[tauri::command]
fn update_game(
    app: tauri::AppHandle,
    payload: UpdateGamePayload,
) -> Result<DashboardData, String> {
    // Capture old game state before updating so we can diff.
    let old_game = settings::load_state(&app)
        .ok()
        .and_then(|s| s.games.into_iter().find(|g| g.id == payload.game.id));

    let old_path_includes: Vec<Vec<String>> = old_game
        .as_ref()
        .map(|g| g.save_paths.iter().map(|e| e.sync_includes.clone()).collect())
        .unwrap_or_default();
    let old_track_changes = old_game.map(|g| g.track_changes).unwrap_or(false);

    let new_save_paths = payload.game.save_paths.clone();
    let new_track_changes = payload.game.track_changes;
    let game_id = payload.game.id.clone();
    let root_drive_folder_id = payload.game.gdrive_folder_id.clone();

    settings::upsert_game(&app, payload.game)?;

    // If track_changes toggled, start or stop the watcher immediately for this session.
    if new_track_changes != old_track_changes {
        if let Err(e) = watcher::handle_track_changes_toggle(&app, &game_id, new_track_changes) {
            eprintln!("[watcher] handle_track_changes_toggle failed in update_game: {e}");
        }
    }

    // Build cleanup tasks: (drive_folder_id, new_includes) per path where includes changed.
    // The cleanup function will remove Drive files not covered by the new inclusion list.
    // When new_includes is empty (sync-all) the cleanup function is a no-op.
    let mut cleanup_tasks: Vec<(String, Vec<String>)> = Vec::new();
    for (i, path_entry) in new_save_paths.iter().enumerate() {
        let old_includes = old_path_includes.get(i).cloned().unwrap_or_default();
        if path_entry.sync_includes == old_includes {
            continue; // No change to this path's filter
        }
        let folder_id = if i == 0 {
            match root_drive_folder_id.clone() {
                Some(id) => id,
                None => continue, // Not synced to Drive yet
            }
        } else {
            match path_entry.gdrive_folder_id.clone() {
                Some(id) => id,
                None => continue, // Path folder not yet created on Drive
            }
        };
        cleanup_tasks.push((folder_id, path_entry.sync_includes.clone()));
    }

    if !cleanup_tasks.is_empty() {
        let app_clone = app.clone();
        let game_id_clone = game_id.clone();
        std::thread::spawn(move || {
            for (folder_id, new_includes) in cleanup_tasks {
                if let Err(e) = sync::cleanup_not_included_from_cloud(
                    &app_clone,
                    &game_id_clone,
                    &folder_id,
                    new_includes,
                ) {
                    eprintln!("[sync] cleanup_not_included_from_cloud failed: {e}");
                }
                // Return value (deleted count) is intentionally ignored here
            }
        });
    }

    let mut state = settings::load_state(&app)?;
    settings::apply_path_overrides(&mut state.games, &state.settings);
    Ok(DashboardData { games: state.games })
}

#[tauri::command]
fn remove_game(app: tauri::AppHandle, game_id: String) -> Result<DashboardData, String> {
    // Capture the Drive folder ID before removing from local state.
    let drive_folder_id = settings::load_state(&app)
        .ok()
        .and_then(|s| s.games.into_iter().find(|g| g.id == game_id))
        .and_then(|g| g.gdrive_folder_id);

    settings::remove_game(&app, &game_id)?;

    // Delete the game's Drive folder (and all saves inside it) in the background.
    if let Some(folder_id) = drive_folder_id {
        let app_clone = app.clone();
        std::thread::spawn(move || {
            match gdrive::delete_drive_file(&app_clone, &folder_id) {
                Ok(_) => println!("[gdrive] Deleted Drive folder for removed game: {folder_id}"),
                Err(e) => eprintln!("[gdrive] Failed to delete Drive folder {folder_id}: {e}"),
            }
        });
    }

    let mut state = settings::load_state(&app)?;
    settings::apply_path_overrides(&mut state.games, &state.settings);
    Ok(DashboardData { games: state.games })
}

#[tauri::command]
fn clear_all_drive_data(app: tauri::AppHandle) -> Result<DashboardData, String> {
    // Delete the entire game-processing-sync root folder on Drive.
    let root_folder_id = gdrive::ensure_root_folder(&app)?;
    gdrive::delete_drive_file(&app, &root_folder_id)?;

    // Clear all cloud metadata from every game in local state.
    let mut state = settings::load_state(&app)?;

    // Also delete all Firestore game documents so the cloud slate is fully clean.
    if let Some(user_id) = crate::gdrive_auth::get_current_user_id(&app) {
        for game in &state.games {
            if let Err(e) = firestore::delete_game(&app, &user_id, &game.id) {
                eprintln!("[firestore] clear_all_drive_data: delete game '{}' failed: {e}", game.id);
            }
        }
    }

    for game in state.games.iter_mut() {
        game.gdrive_folder_id = None;
        game.total_play_time_seconds = 0;
        game.cloud_storage_bytes = None;
        game.last_cloud_modified = None;
    }
    state.last_cloud_library_modified = None;
    settings::save_state(&app, &state)?;

    println!("[gdrive] Cleared all Drive data for current account");
    settings::apply_path_overrides(&mut state.games, &state.settings);
    Ok(DashboardData { games: state.games })
}

#[tauri::command]
async fn check_auth_status(app: tauri::AppHandle) -> Result<AuthStatus, String> {
    tokio::task::spawn_blocking(move || gdrive_auth::check_auth_status(&app))
        .await
        .map_err(|e| e.to_string())?
}

/// Receive tokens from the frontend after tauri-plugin-google-auth sign-in.
#[tauri::command]
fn save_auth_tokens(
    app: tauri::AppHandle,
    payload: SaveTokensPayload,
) -> Result<AuthStatus, String> {
    let status = gdrive_auth::save_tokens_from_plugin(&app, payload)?;

    // After every login: restore library + settings from Firestore (with Drive migration
    // fallback), then sync all save files.
    let app_clone = app.clone();
    std::thread::spawn(move || {
        let mut library_changed = false;

        match settings::fetch_all_from_firestore(&app_clone) {
            Ok(true) => {
                println!("[firestore] Post-login: library + settings restored");
                library_changed = true;
                let _ = tauri::Emitter::emit(&app_clone, "library-restored", ());
            }
            Ok(false) => println!("[firestore] Post-login: no data in Firestore yet"),
            Err(e) => eprintln!("[firestore] Post-login restore failed: {e}"),
        }

        // Register / update device record after successful login.
        devices::register_current_device(&app_clone);

        // Restore device-local path overrides from Firestore (disaster recovery)
        // or push existing local overrides up for the first time (migration).
        if let Err(e) = settings::load_and_merge_device_paths(&app_clone) {
            eprintln!("[firestore] Post-login: load_and_merge_device_paths failed: {e}");
        }

        // Restore device-local exe-path overrides from Firestore (disaster recovery).
        if let Err(e) = settings::load_and_merge_device_exe_paths(&app_clone) {
            eprintln!("[firestore] Post-login: load_and_merge_device_exe_paths failed: {e}");
        }

        // Sync all game saves from Drive (picks up cloud-side changes).
        match sync::sync_all_games(&app_clone) {
            Ok(results) => {
                let downloaded: u32 = results.iter().map(|r| r.downloaded).sum();
                let uploaded: u32 = results.iter().map(|r| r.uploaded).sum();
                println!(
                    "[sync] Post-login sync done — {} games, {} downloaded, {} uploaded",
                    results.len(),
                    downloaded,
                    uploaded
                );
                let _ = tauri::Emitter::emit(&app_clone, "post-login-sync-completed", ());
            }
            Err(e) => {
                eprintln!("[sync] Post-login sync_all_games failed: {e}");
                let _ = tauri::Emitter::emit(&app_clone, "post-login-sync-completed", ());
            }
        }

        let _ = library_changed; // suppress unused warning when library was already up-to-date
    });

    Ok(status)
}

/// Return OAuth client credentials so the frontend can pass them to the plugin.
#[tauri::command]
fn get_oauth_credentials() -> Result<OAuthCredentials, String> {
    let client_id = gdrive_auth::get_client_id()?;
    let client_secret = gdrive_auth::get_client_secret();
    Ok(OAuthCredentials {
        client_id,
        client_secret,
    })
}

#[tauri::command]
fn logout(app: tauri::AppHandle) -> Result<AuthStatus, String> {
    gdrive_auth::logout(&app)
}

#[tauri::command]
fn get_google_user_info(app: tauri::AppHandle) -> Result<GoogleUserInfo, String> {
    gdrive_auth::get_google_user_info(&app)
}

#[tauri::command]
fn get_admin_config(app: tauri::AppHandle) -> Result<AdminConfig, String> {
    admin::get_admin_config_cmd(&app)
}

#[tauri::command]
fn update_admin_config(app: tauri::AppHandle, config: AdminConfig) -> Result<AdminConfig, String> {
    admin::update_admin_config_cmd(&app, config)
}

#[tauri::command]
fn list_users(app: tauri::AppHandle) -> Result<Vec<UserProfile>, String> {
    admin::list_users_cmd(&app)
}

#[tauri::command]
fn update_user_role(
    app: tauri::AppHandle,
    user_id: String,
    role: UserRole,
) -> Result<Vec<UserProfile>, String> {
    admin::update_user_role_cmd(&app, user_id, role)
}

// ── Settings commands ─────────────────────────────────────

#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> Result<AppSettings, String> {
    settings::get_settings(&app)
}

#[tauri::command]
fn update_settings(app: tauri::AppHandle, settings: AppSettings) -> Result<AppSettings, String> {
    settings::update_settings(&app, settings)
}

// ── Path validation commands ──────────────────────────────

#[tauri::command]
fn expand_save_path(path: String) -> String {
    settings::expand_env_vars(&path)
}

/// Replace absolute path prefixes with portable env-var tokens (e.g. `%PROGRAMFILES%\...`).
/// Mirrors the server-side `contract_env_vars` used on save, so the frontend can show
/// the portable form immediately after a file-picker selection.
#[tauri::command]
fn contract_path(path: String) -> String {
    settings::contract_path(&path)
}

#[tauri::command]
fn validate_save_paths(app: tauri::AppHandle) -> Result<Vec<PathValidation>, String> {
    settings::validate_save_paths(&app)
}

#[tauri::command]
fn get_browse_default_path(app: tauri::AppHandle) -> Result<Option<String>, String> {
    settings::get_browse_default_path(&app)
}

// ── Save info commands ─────────────────────────────────────

#[tauri::command]
fn get_save_info(app: tauri::AppHandle, game_id: String) -> Result<SaveInfo, String> {
    sync::get_save_info(&app, &game_id)
}

// ── Sync commands ─────────────────────────────────────────

/// Validate a game logo (file ≤ 2 MB; URL download ≤ 2 MB), upload it to the
/// game's Google Drive folder as `logo.<ext>`, and return the hosted preview URL.
#[tauri::command]
async fn upload_game_logo(
    app: tauri::AppHandle,
    game_id: String,
    logo_source: String,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || gdrive::upload_game_logo(&app, &game_id, &logo_source))
        .await
        .map_err(|e| format!("Logo upload task failed: {e}"))?
}

/// Return the byte size of a local file path. Used by the frontend to validate
/// thumbnail image size before upload.
#[tauri::command]
fn get_file_size(path: String) -> Result<u64, String> {
    std::fs::metadata(&path)
        .map(|m| m.len())
        .map_err(|e| format!("Could not read file metadata: {e}"))
}

/// Download a Drive logo by thumbnail identifier and return it as a base64 data URL.
/// Accepts `drive-file:{fileId}` (current) and legacy `gdrive-img://` format.
#[tauri::command]
async fn get_logo_data_url(
    app: tauri::AppHandle,
    thumbnail: String,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let file_id = if let Some(id) = thumbnail.strip_prefix("drive-file:") {
            id.to_string()
        } else if let Some(id) = thumbnail.strip_prefix("gdrive-img://localhost/") {
            id.to_string()
        } else if let Some(id) = thumbnail.strip_prefix("gdrive-img://") {
            id.to_string()
        } else {
            return Err(format!("Not a Drive thumbnail: {thumbnail}"));
        };

        let (bytes, content_type) = gdrive::download_logo_by_file_id(&app, &file_id)?;
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(format!("data:{content_type};base64,{b64}"))
    })
    .await
    .map_err(|e| format!("Logo fetch task failed: {e}"))?
}

// ── Drive file management commands ────────────────────────

#[tauri::command]
async fn list_game_drive_files_flat(
    app: tauri::AppHandle,
    game_id: String,
) -> Result<Vec<DriveFileFlatItem>, String> {
    tokio::task::spawn_blocking(move || drive_mgmt::list_game_drive_files_flat(&app, &game_id))
        .await
        .map_err(|e| format!("List flat drive files task failed: {e}"))?
}

#[tauri::command]
async fn list_game_drive_files(
    app: tauri::AppHandle,
    game_id: String,
    folder_id: Option<String>,
) -> Result<Vec<DriveFileItem>, String> {
    tokio::task::spawn_blocking(move || {
        drive_mgmt::list_game_drive_files(&app, &game_id, folder_id.as_deref())
    })
    .await
    .map_err(|e| format!("List drive files task failed: {e}"))?
}

#[tauri::command]
async fn rename_game_drive_file(
    app: tauri::AppHandle,
    game_id: String,
    file_id: String,
    old_name: String,
    new_name: String,
    is_folder: bool,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        drive_mgmt::rename_game_drive_file(&app, &game_id, &file_id, &old_name, &new_name, is_folder)
    })
    .await
    .map_err(|e| format!("Rename task failed: {e}"))?
}

#[tauri::command]
async fn move_game_drive_file(
    app: tauri::AppHandle,
    game_id: String,
    file_id: String,
    file_name: String,
    new_parent_id: String,
    old_parent_id: String,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        drive_mgmt::move_game_drive_file(
            &app,
            &game_id,
            &file_id,
            &file_name,
            &new_parent_id,
            &old_parent_id,
        )
    })
    .await
    .map_err(|e| format!("Move task failed: {e}"))?
}

#[tauri::command]
async fn delete_game_drive_file(
    app: tauri::AppHandle,
    game_id: String,
    file_id: String,
    file_name: String,
    is_folder: bool,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        drive_mgmt::delete_game_drive_file(&app, &game_id, &file_id, &file_name, is_folder)
    })
    .await
    .map_err(|e| format!("Delete task failed: {e}"))?
}

#[tauri::command]
async fn create_version_backup(
    app: tauri::AppHandle,
    game_id: String,
    label: Option<String>,
) -> Result<DriveVersionBackup, String> {
    tokio::task::spawn_blocking(move || drive_mgmt::create_version_backup(&app, &game_id, label))
        .await
        .map_err(|e| format!("Create backup task failed: {e}"))?
}

#[tauri::command]
async fn list_version_backups(
    app: tauri::AppHandle,
    game_id: String,
) -> Result<Vec<DriveVersionBackup>, String> {
    tokio::task::spawn_blocking(move || drive_mgmt::list_version_backups(&app, &game_id))
        .await
        .map_err(|e| format!("List backups task failed: {e}"))?
}

#[tauri::command]
async fn restore_version_backup(
    app: tauri::AppHandle,
    game_id: String,
    backup_folder_id: String,
) -> Result<SyncResult, String> {
    tokio::task::spawn_blocking(move || {
        drive_mgmt::restore_version_backup(&app, &game_id, &backup_folder_id)
    })
    .await
    .map_err(|e| format!("Restore backup task failed: {e}"))?
}

#[tauri::command]
async fn delete_version_backup(
    app: tauri::AppHandle,
    game_id: String,
    backup_folder_id: String,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        drive_mgmt::delete_version_backup(&app, &game_id, &backup_folder_id)
    })
    .await
    .map_err(|e| format!("Delete backup task failed: {e}"))?
}

#[tauri::command]
async fn sync_game(app: tauri::AppHandle, game_id: String) -> Result<SyncResult, String> {
    tokio::task::spawn_blocking(move || sync::sync_game(&app, &game_id))
        .await
        .map_err(|e| format!("Sync task failed: {e}"))?
}

#[tauri::command]
async fn sync_all_games(app: tauri::AppHandle) -> Result<Vec<SyncResult>, String> {
    tokio::task::spawn_blocking(move || sync::sync_all_games(&app))
        .await
        .map_err(|e| format!("Sync all task failed: {e}"))?
}

#[tauri::command]
async fn check_sync_structure_diff(
    app: tauri::AppHandle,
    game_id: String,
) -> Result<SyncStructureDiff, String> {
    tokio::task::spawn_blocking(move || sync::check_sync_structure_diff(&app, &game_id))
        .await
        .map_err(|e| format!("Diff check task failed: {e}"))?
}

#[tauri::command]
async fn restore_from_cloud(
    app: tauri::AppHandle,
    game_id: String,
) -> Result<SyncResult, String> {
    tokio::task::spawn_blocking(move || sync::restore_from_cloud(&app, &game_id))
        .await
        .map_err(|e| format!("Restore task failed: {e}"))?
}

#[tauri::command]
async fn push_to_cloud(
    app: tauri::AppHandle,
    game_id: String,
) -> Result<SyncResult, String> {
    tokio::task::spawn_blocking(move || sync::push_to_cloud(&app, &game_id))
        .await
        .map_err(|e| format!("Push task failed: {e}"))?
}

#[tauri::command]
async fn clean_excluded_drive_files(
    app: tauri::AppHandle,
    game_id: String,
) -> Result<DashboardData, String> {
    tokio::task::spawn_blocking(move || {
        sync::clean_not_included_from_cloud_all_paths(&app, &game_id)?;
        let mut state = settings::load_state(&app)?;
        settings::apply_path_overrides(&mut state.games, &state.settings);
        Ok(DashboardData { games: state.games })
    })
    .await
    .map_err(|e| format!("Clean excluded task failed: {e}"))?
}

// ── Launcher command ──────────────────────────────────────

#[tauri::command]
fn launch_game(app: tauri::AppHandle, game_id: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    let state = settings::load_state(&app)?;
    let game = settings::find_game(&state, &game_id)?;

    let raw_path = game
        .exe_path
        .as_deref()
        .ok_or_else(|| "No executable path configured for this game.".to_string())?;

    let full_path = settings::expand_env_vars(raw_path);

    println!("[launcher] Launching '{}' at: {}", game.name, full_path);

    app.opener()
        .open_path(&full_path, None::<&str>)
        .map_err(|e| format!("Failed to launch game: {e}"))?;

    // Arm the process watcher if this game has tracking enabled and an exe_name.
    // The watcher is armed here (on Play) rather than at startup for games with a
    // valid local exe_path, so we don't poll unnecessarily on other devices.
    if game.track_changes {
        if let Some(exe) = &game.exe_name {
            if !exe.is_empty() {
                watcher::arm_on_launch(&app, &game_id, exe);
            }
        }
    }

    Ok(())
}

// ── Watcher commands ──────────────────────────────────────

#[tauri::command]
fn toggle_track_changes(
    app: tauri::AppHandle,
    game_id: String,
    enabled: bool,
) -> Result<DashboardData, String> {
    // Update the game entry first
    let mut state = settings::update_game_field(&app, &game_id, |g| {
        g.track_changes = enabled;
    })?;

    // Start or stop the watcher
    watcher::handle_track_changes_toggle(&app, &game_id, enabled)?;

    settings::apply_path_overrides(&mut state.games, &state.settings);
    Ok(DashboardData { games: state.games })
}

#[tauri::command]
fn toggle_auto_sync(
    app: tauri::AppHandle,
    game_id: String,
    enabled: bool,
) -> Result<DashboardData, String> {
    let mut state = settings::update_game_field(&app, &game_id, |g| {
        g.auto_sync = enabled;
    })?;

    settings::apply_path_overrides(&mut state.games, &state.settings);
    Ok(DashboardData { games: state.games })
}

// ── Device management commands ────────────────────────────

#[tauri::command]
async fn get_devices(app: tauri::AppHandle) -> Result<Vec<DeviceInfo>, String> {
    tokio::task::spawn_blocking(move || devices::get_devices_cmd(&app)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn rename_device(
    app: tauri::AppHandle,
    device_id: String,
    name: String,
) -> Result<Vec<DeviceInfo>, String> {
    tokio::task::spawn_blocking(move || devices::rename_device_cmd(&app, device_id, name))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn remove_device(
    app: tauri::AppHandle,
    device_id: String,
) -> Result<Vec<DeviceInfo>, String> {
    tokio::task::spawn_blocking(move || devices::remove_device_cmd(&app, device_id))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_google_auth::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // Initialize the WatcherManager as managed state
            let manager = watcher::WatcherManager::new(app.handle().clone());
            app.manage(Arc::new(Mutex::new(manager)));

            // Start watchers for games that have tracking enabled
            watcher::init_watchers(app.handle());

            // Register this device in Firestore (if already authenticated — e.g. on subsequent launches)
            let app_for_device = app.handle().clone();
            std::thread::spawn(move || {
                devices::register_current_device(&app_for_device);
            });

            // Set up system tray icon and context menu
            tray::setup_tray(app).map_err(|e| e.to_string())?;

            // If "start minimised" is enabled, hide the main window
            if let Ok(s) = settings::get_settings(app.handle()) {
                if s.start_minimised {
                    if let Some(win) = app.get_webview_window("main") {
                        let _ = win.hide();
                    }
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_dashboard,
            add_manual_game,
            update_game,
            remove_game,
            clear_all_drive_data,
            check_auth_status,
            save_auth_tokens,
            get_oauth_credentials,
            logout,
            get_google_user_info,
            get_admin_config,
            update_admin_config,
            list_users,
            update_user_role,
            get_settings,
            update_settings,
            get_save_info,
            sync_game,
            sync_all_games,
            check_sync_structure_diff,
            restore_from_cloud,
            push_to_cloud,
            clean_excluded_drive_files,
            toggle_track_changes,
            toggle_auto_sync,
            launch_game,
            validate_save_paths,
            get_browse_default_path,
            expand_save_path,
            contract_path,
            upload_game_logo,
            get_file_size,
            get_logo_data_url,
            list_game_drive_files_flat,
            list_game_drive_files,
            rename_game_drive_file,
            move_game_drive_file,
            delete_game_drive_file,
            create_version_backup,
            list_version_backups,
            restore_version_backup,
            delete_version_backup,
            get_devices,
            rename_device,
            remove_device,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Hide the window instead of closing — app stays in system tray
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
