mod adapter;
mod runtime;
mod native_dialogs;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WindowEvent};

use crate::runtime::{AppState, SetupRequest};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Err(error) = tauri::webview_version() {
        native_dialogs::show_startup_error(&format!("无法启动本机界面：{error}\n\n请安装 Microsoft Edge WebView2 Runtime 后重试。\n官方下载：https://developer.microsoft.com/microsoft-edge/webview2/"));
        return;
    }
    let result = tauri::Builder::default()
        .manage(AppState::new())
        .setup(|app| {
            install_tray(app.handle())?;
            install_close_behavior(app.handle())?;
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let _ = tauri::async_runtime::spawn_blocking(move || runtime::ensure_boot(&handle))
                    .await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            participant_bootstrap,
            participant_poll,
            participant_command,
            participant_setup,
            participant_details,
            participant_quit,
            participant_app_info,
            participant_pick_file,
            participant_open_reference,
            participant_settings_preview,
            participant_import_settings,
            participant_switch_config,
            participant_pending_settings
        ])
        .run(tauri::generate_context!());
    if let Err(error) = result {
        native_dialogs::show_startup_error(&format!("RustLanBus 启动失败：{error}\n配置文件会保留。请确认运行时和当前程序文件完整。"));
    }
}

fn install_close_behavior(app: &tauri::AppHandle) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };
    let window_handle = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = window_handle.hide();
        }
    });
    Ok(())
}

fn install_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("RustLanBus")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "quit" => quit_application(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn quit_application(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let check = app.clone();
        let allowed = tauri::async_runtime::spawn_blocking(move || runtime::request_quit(&check)).await;
        match allowed {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => return,
            Ok(Err(error)) => { native_dialogs::show_startup_error(&error); return; }
            Err(error) => { native_dialogs::show_startup_error(&error.to_string()); return; }
        }
        let app_for_stop = app.clone();
        let _ =
            tauri::async_runtime::spawn_blocking(move || runtime::shutdown(&app_for_stop)).await;
        app.exit(0);
    });
}

#[tauri::command]
async fn participant_bootstrap(app: tauri::AppHandle) -> Result<runtime::BootReport, String> {
    tauri::async_runtime::spawn_blocking(move || runtime::ensure_boot(&app))
        .await
        .map_err(|_| "读取本机配置时后台任务中断。".to_owned())
}

#[tauri::command]
async fn participant_poll(app: tauri::AppHandle) -> Result<adapter::ViewState, String> {
    tauri::async_runtime::spawn_blocking(move || runtime::poll(&app))
        .await
        .map_err(|_| "读取参与状态时后台任务中断。".to_owned())
}

#[tauri::command]
async fn participant_command(
    app: tauri::AppHandle,
    action: adapter::UiAction,
) -> Result<adapter::CommandView, String> {
    tauri::async_runtime::spawn_blocking(move || runtime::command(&app, action))
        .await
        .map_err(|_| "发送参与命令时后台任务中断。".to_owned())?
}

#[tauri::command]
async fn participant_setup(
    app: tauri::AppHandle,
    request: SetupRequest,
) -> Result<runtime::BootReport, String> {
    tauri::async_runtime::spawn_blocking(move || runtime::setup_and_start(&app, request))
        .await
        .map_err(|_| "创建本机配置时后台任务中断。".to_owned())?
}

#[tauri::command]
async fn participant_details(app: tauri::AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || runtime::details(&app))
        .await
        .map_err(|_| "导出诊断摘要时后台任务中断。".to_owned())?
}

#[tauri::command]
async fn participant_quit(app: tauri::AppHandle) -> Result<(), String> {
    let check = app.clone();
    let allowed = tauri::async_runtime::spawn_blocking(move || runtime::request_quit(&check))
        .await.map_err(|error| error.to_string())??;
    if !allowed { return Ok(()); }
    let stop = app.clone();
    tauri::async_runtime::spawn_blocking(move || runtime::shutdown(&stop)).await.map_err(|error| error.to_string())?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn participant_app_info(app: tauri::AppHandle) -> runtime::AppInfo { runtime::app_info(&app) }

#[tauri::command]
async fn participant_pick_file(app: tauri::AppHandle, kind: String) -> Result<Option<String>,String> {
    tauri::async_runtime::spawn_blocking(move || runtime::pick_file(&app, &kind)).await.map_err(|error|error.to_string())?
}

#[tauri::command]
async fn participant_open_reference(key: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || native_dialogs::open_reference(&key))
        .await.map_err(|error| error.to_string())?
}

#[tauri::command]
async fn participant_settings_preview(input: String, destination: Option<String>, mode: rustlanbus_engine::participant::management::ImportMode) -> Result<rustlanbus_engine::participant::management::BundlePreview,String> {
    tauri::async_runtime::spawn_blocking(move || runtime::settings_preview(input.into(), destination.filter(|s|!s.is_empty()).map(Into::into), mode)).await.map_err(|error|error.to_string())?
}

#[tauri::command]
async fn participant_import_settings(app: tauri::AppHandle, input: String, destination: String, options: rustlanbus_engine::participant::management::ImportOptions, discard_unsaved: bool) -> Result<runtime::SettingsTransition,String> {
    tauri::async_runtime::spawn_blocking(move || runtime::import_settings(&app, std::path::Path::new(&input), std::path::Path::new(&destination), options, discard_unsaved)).await.map_err(|error|error.to_string())?
}

#[tauri::command]
async fn participant_switch_config(app: tauri::AppHandle, path: String, discard_unsaved: bool) -> Result<runtime::BootReport,String> {
    tauri::async_runtime::spawn_blocking(move || runtime::switch_config(&app, std::path::Path::new(&path), discard_unsaved)).await.map_err(|error|error.to_string())?
}

#[tauri::command]
async fn participant_pending_settings(app: tauri::AppHandle) -> Result<Vec<rustlanbus_engine::participant::management::UnsavedChange>,String> {
    tauri::async_runtime::spawn_blocking(move || runtime::pending_changes(&app)).await.map_err(|error|error.to_string())?
}
