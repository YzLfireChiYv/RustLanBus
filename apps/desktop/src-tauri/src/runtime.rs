//! One desktop owner for the participant controller.
//!
//! Command, startup, and shutdown work clone or remove the handle under a short
//! lock, then wait outside that lock. The interface thread does not keep the
//! owner mutex while the controller saves or joins.

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rustlanbus_engine::participant::management::{
    self, BundlePreview, ImportMode, ImportOptions, ImportResult, ManagementResult, UnsavedChange,
};
use rustlanbus_engine::participant::{
    ParticipantConfig, RunningParticipant, create_config_file, load_config, start,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::adapter::{self, CommandView, ViewState};

pub struct AppState {
    operation: Mutex<()>,
    inner: Mutex<Inner>,
    ready: Condvar,
}

struct Inner {
    owner: Option<RunningParticipant>,
    boot: Boot,
    shutdown_requested: bool,
    exit_inflight: bool,
}

enum Boot {
    NotStarted,
    Starting,
    Finished(Box<BootReport>),
    Stopping,
    Stopped,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootReport {
    pub phase: &'static str,
    pub config_path: String,
    pub state: ViewState,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetupRequest {
    pub name: String,
    pub bind: String,
    pub existing_path: Option<String>,
    pub new_path: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsTransition {
    pub boot: BootReport,
    pub imported: Option<ImportResult>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub build_id: String,
    pub engine_build_id: String,
    pub config_path: String,
    pub executable_path: String,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            operation: Mutex::new(()),
            inner: Mutex::new(Inner {
                owner: None,
                boot: Boot::NotStarted,
                shutdown_requested: false,
                exit_inflight: false,
            }),
            ready: Condvar::new(),
        }
    }
}

pub fn ensure_boot(app: &AppHandle) -> BootReport {
    let state = app.state::<AppState>();
    loop {
        let mut inner = state.inner.lock().expect("participant owner");
        match &inner.boot {
            Boot::Finished(report) => {
                let mut report = (**report).clone();
                let handle = inner.owner.as_ref().map(RunningParticipant::handle);
                drop(inner);
                if let Some(handle) = handle {
                    report.state = adapter::map_snapshot(&handle.snapshot());
                    append_boot_message(&mut report);
                }
                return report;
            }
            Boot::Starting => {
                inner = state.ready.wait(inner).expect("participant boot");
                drop(inner);
            }
            Boot::Stopping | Boot::Stopped => return stopped_report(),
            Boot::NotStarted => {
                inner.boot = Boot::Starting;
                break;
            }
        }
    }
    let (report, owner) = boot_from_disk(app);
    finish_boot(&state, &report, owner);
    report
}

pub fn setup_and_start(app: &AppHandle, request: SetupRequest) -> Result<BootReport, String> {
    let state = app.state::<AppState>();
    let _operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let attempted_path = nonempty(request.existing_path.as_deref())
        .or_else(|| nonempty(request.new_path.as_deref()))
        .map(str::to_owned);
    claim_setup(&state)?;
    let result = default_config_path(app).and_then(|default| {
        let directory = default.parent().expect("config directory");
        let prepared = prepare_setup(&default, request)?;
        launch_setup(directory, prepared)
    });
    match result {
        Ok((report, owner)) => {
            finish_boot(&state, &report, Some(owner));
            Ok(report)
        }
        Err(error) => {
            let report = BootReport {
                phase: "setup",
                config_path: attempted_path.unwrap_or_else(|| {
                    default_config_path(app)
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                }),
                state: adapter::unavailable_view(error.clone()),
                message: Some(error.clone()),
            };
            finish_boot(&state, &report, None);
            Err(error)
        }
    }
}

fn claim_setup(state: &AppState) -> Result<(), String> {
    let mut inner = state.inner.lock().expect("participant owner");
    if inner.shutdown_requested || inner.exit_inflight {
        return Err("程序正在退出，无法再启动配置。".into());
    }
    if inner.owner.is_some() {
        return Err("参与控制器已经运行。请直接使用当前窗口里的设置。".into());
    }
    if matches!(inner.boot, Boot::Starting) {
        return Err("本机配置正在启动，请等待当前操作完成。".into());
    }
    inner.boot = Boot::Starting;
    Ok(())
}

fn finish_boot(state: &AppState, report: &BootReport, owner: Option<RunningParticipant>) {
    let mut inner = state.inner.lock().expect("participant owner");
    // A boot/setup claim is exclusive. Shutdown waits for it before taking the
    // resulting owner, so an existing owner is never replaced under this lock.
    debug_assert!(inner.owner.is_none());
    inner.owner = owner;
    inner.boot = Boot::Finished(Box::new(report.clone()));
    state.ready.notify_all();
}

struct PreparedSetup {
    path: PathBuf,
    new_config: Option<ParticipantConfig>,
}

fn prepare_setup(default: &Path, request: SetupRequest) -> Result<PreparedSetup, String> {
    let existing = nonempty(request.existing_path.as_deref());
    let new_path = nonempty(request.new_path.as_deref());
    if existing.is_some() && new_path.is_some() {
        return Err("打开已有配置和创建新配置需要分开选择。".into());
    }
    let path = existing
        .or(new_path)
        .map_or_else(|| default.to_path_buf(), PathBuf::from);
    if !path.is_absolute() {
        return Err("配置需要使用完整的绝对路径。".into());
    }
    let config = if existing.is_some() {
        load_config(&path)?
    } else {
        match fs::metadata(&path) {
            Ok(_) => {
                return Err(format!(
                    "配置文件 {} 已经存在，本次创建已停止，原文件不会被覆盖。请用高级设置打开继续使用；后续设置会写回所选文件。",
                    path.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("无法检查配置路径 {}：{error}", path.display())),
        }
        ParticipantConfig::create(request.name.trim(), parse_bind(request.bind.trim())?)?
    };
    // The selected existing file remains the active writable configuration.
    // Controller startup restores its saved intent. Setup prepares local data.
    config.validate()?;
    Ok(PreparedSetup {
        path,
        new_config: existing.is_none().then_some(config),
    })
}

fn launch_setup(
    directory: &Path,
    prepared: PreparedSetup,
) -> Result<(BootReport, RunningParticipant), String> {
    let PreparedSetup { path, new_config } = prepared;
    if let Some(config) = new_config {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("无法创建配置目录 {}：{error}", parent.display()))?;
        }
        create_config_file(&path, &config)?;
    }
    let running = start(&path)?;
    let handle = running.handle();
    let mut notes = Vec::new();
    if let Err(error) = save_bootstrap_path(directory, &path) {
        notes.push(format!(
            "当前配置已启动，但下次打开所用的配置路径未能保存。{error}"
        ));
    }
    let mut report = BootReport {
        phase: "running",
        config_path: path.display().to_string(),
        state: adapter::map_snapshot(&handle.snapshot()),
        message: (!notes.is_empty()).then(|| notes.join(" ")),
    };
    append_boot_message(&mut report);
    Ok((report, running))
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn append_boot_message(report: &mut BootReport) {
    append_startup_notice(&mut report.state, report.message.as_deref());
}

fn append_startup_notice(view: &mut ViewState, notice: Option<&str>) {
    if let Some(notice) = notice {
        let message = format!("启动提示：{notice}");
        view.message = Some(match view.message.take() {
            Some(current) => format!("{current} {message}"),
            None => message,
        });
    }
}

pub fn poll(app: &AppHandle) -> ViewState {
    let (handle, note) = {
        let state = app.state::<AppState>();
        let inner = state.inner.lock().expect("participant owner");
        (
            inner.owner.as_ref().map(RunningParticipant::handle),
            startup_notice(&inner),
        )
    };
    match handle {
        Some(handle) => {
            let mut view = adapter::map_snapshot(&handle.snapshot());
            append_startup_notice(&mut view, note.as_deref());
            view
        }
        None => adapter::unavailable_view("配置还没有启动。请先完成本机设置。"),
    }
}

pub fn command(app: &AppHandle, action: adapter::UiAction) -> Result<CommandView, String> {
    let state = app.state::<AppState>();
    let _operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (handle, note) = {
        let inner = state.inner.lock().expect("participant owner");
        if inner.shutdown_requested
            || inner.exit_inflight
            || !matches!(inner.boot, Boot::Finished(_))
        {
            return Err("配置正在切换或程序正在退出，本次操作没有发出。".into());
        }
        let handle = inner
            .owner
            .as_ref()
            .map(RunningParticipant::handle)
            .ok_or_else(|| "参与控制器还没有启动，操作没有发出。".to_owned())?;
        (handle, startup_notice(&inner))
    };
    let outcome = handle.command(action.into_command()?)?;
    let mut result = adapter::map_outcome(outcome);
    append_startup_notice(&mut result.state, note.as_deref());
    Ok(result)
}

fn startup_notice(inner: &Inner) -> Option<String> {
    match &inner.boot {
        Boot::Finished(report) if report.phase == "running" => report.message.clone(),
        _ => None,
    }
}

pub fn details(app: &AppHandle) -> Result<String, String> {
    let state = app.state::<AppState>();
    let _operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let handle = {
        let state = app.state::<AppState>();
        let inner = state.inner.lock().expect("participant owner");
        inner
            .owner
            .as_ref()
            .map(RunningParticipant::handle)
            .ok_or_else(|| "参与控制器还没有启动，没有可以导出的状态。".to_owned())?
    };
    let info = app_info(app);
    Ok(format!(
        "RustLanBus {}\nDesktop build: {}\nEngine build: {}\nProgram: {}\nConfiguration: {}\n\n{}",
        info.version,
        info.build_id,
        info.engine_build_id,
        info.executable_path,
        info.config_path,
        adapter::sanitized_details(&handle.snapshot())
    ))
}

pub fn app_info(app: &AppHandle) -> AppInfo {
    let state = app.state::<AppState>();
    let inner = state.inner.lock().expect("participant owner");
    let config_path = match &inner.boot {
        Boot::Finished(report) => report.config_path.clone(),
        _ => String::new(),
    };
    AppInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        build_id: option_env!("RLB_DESKTOP_BUILD_ID")
            .unwrap_or("development")
            .into(),
        engine_build_id: rustlanbus_engine::participant::BUILD_ID.into(),
        config_path,
        executable_path: std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("unavailable: {error}")),
    }
}

pub fn pick_file(app: &AppHandle, kind: &str) -> Result<Option<String>, String> {
    let state = app.state::<AppState>();
    let _operation = state.operation.lock().map_err(|_| "设置操作状态不可用。".to_owned())?;
    {
        let inner = state.inner.lock().map_err(|_| "本机状态不可用。".to_owned())?;
        if inner.shutdown_requested || inner.exit_inflight { return Err("程序正在退出。".into()); }
    }
    crate::native_dialogs::pick_file(app, kind)
}

pub fn settings_preview(
    input: PathBuf,
    destination: Option<PathBuf>,
    mode: ImportMode,
) -> Result<BundlePreview, String> {
    management::preview_bundle(&input, destination.as_deref(), mode)
}

pub fn pending_changes(app: &AppHandle) -> Result<Vec<UnsavedChange>, String> {
    let state = app.state::<AppState>();
    let _operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let handle = {
        let inner = state.inner.lock().expect("participant owner");
        if inner.exit_inflight || inner.shutdown_requested {
            return Err("程序正在退出。".into());
        }
        inner.owner.as_ref().map(RunningParticipant::handle)
    };
    match handle {
        Some(handle) => query_pending(&handle),
        None => Ok(Vec::new()),
    }
}

fn query_pending(
    handle: &rustlanbus_engine::participant::ParticipantHandle,
) -> Result<Vec<UnsavedChange>, String> {
    let outcome = handle.command(rustlanbus_engine::participant::Command::CheckPendingSettings)?;
    match outcome.management {
        Some(ManagementResult::PendingChanges { changes }) => Ok(changes),
        _ => Err("共同控制层未返回保存状态，操作已停止。".into()),
    }
}

fn pending_text(changes: &[UnsavedChange]) -> String {
    changes
        .iter()
        .map(|change| {
            let label = match change.kind {
                "configuration" => "本机配置",
                "global" => "全局规则",
                "room" => "房间规则",
                _ => "保存状态未确认",
            };
            format!(
                "{label}：{}{}",
                change.path.display(),
                change
                    .detail
                    .as_ref()
                    .map(|detail| format!("\n  {detail}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn finished_path(inner: &Inner) -> Option<PathBuf> {
    match &inner.boot {
        Boot::Finished(report) if !report.config_path.is_empty() => {
            Some(PathBuf::from(&report.config_path))
        }
        _ => None,
    }
}

struct Previous {
    path: Option<PathBuf>,
    owner: Option<RunningParticipant>,
    registered_startup: bool,
}

/// The caller holds operation. The owner lock is released during every query,
/// stop, file transaction and startup, including failure recovery.
fn take_for_transition(state: &AppState, discard_unsaved: bool) -> Result<Previous, String> {
    let handle = {
        let inner = state.inner.lock().expect("participant owner");
        if inner.shutdown_requested || inner.exit_inflight {
            return Err("程序正在退出，无法切换配置。".into());
        }
        if matches!(inner.boot, Boot::Starting | Boot::Stopping) {
            return Err("已有配置操作正在进行，请稍后重试。".into());
        }
        inner.owner.as_ref().map(RunningParticipant::handle)
    };
    let registered_startup = handle
        .as_ref()
        .is_some_and(|handle| handle.snapshot().login_startup.registered == Some(true));
    if let Some(handle) = handle {
        if !discard_unsaved {
            let changes = query_pending(&handle)?;
            if !changes.is_empty() {
                return Err(format!(
                    "有未保存的内容。请返回处理，或明确放弃后再切换／导入。\n{}",
                    pending_text(&changes)
                ));
            }
        }
    }
    let mut inner = state.inner.lock().expect("participant owner");
    if inner.shutdown_requested || inner.exit_inflight {
        return Err("程序正在退出，无法切换配置。".into());
    }
    let path = finished_path(&inner);
    inner.boot = Boot::Starting;
    Ok(Previous {
        path,
        owner: inner.owner.take(),
        registered_startup,
    })
}

fn note_previous_registration(report: &mut BootReport, previous: &Previous, target: &Path) {
    if !previous.registered_startup
        || !previous.path.as_ref().is_some_and(|old| {
            old.as_path() != target
                && !std::fs::canonicalize(old)
                    .ok()
                    .zip(std::fs::canonicalize(target).ok())
                    .is_some_and(|(old, new)| old == new)
        })
    {
        return;
    }
    let notice =
        "原配置仍有 Windows 登录自启登记，本次切换没有更改该登记；需要时请显式更新其自启设置。";
    report.message = Some(match report.message.take() {
        Some(old) => format!("{old} {notice}"),
        None => notice.into(),
    });
    append_startup_notice(&mut report.state, Some(notice));
}

fn bootstrap_directory(app: &AppHandle) -> Result<PathBuf, String> {
    default_config_path(app)?
        .parent()
        .map(Path::to_path_buf)
        .ok_or("本机配置目录无法确定。".into())
}

fn start_existing(
    directory: &Path,
    path: &Path,
) -> Result<(BootReport, RunningParticipant), String> {
    launch_setup(
        directory,
        PreparedSetup {
            path: path.to_path_buf(),
            new_config: None,
        },
    )
}

fn recover_previous(
    state: &AppState,
    directory: &Path,
    previous: Option<&Path>,
    attempted: &Path,
    error: &str,
) -> BootReport {
    if let Some(previous) = previous {
        match start_existing(directory, previous) {
            Ok((mut report, owner)) => {
                let notice = format!(
                    "操作未完成，已继续使用原配置 {}。{error}",
                    previous.display()
                );
                report.message = Some(match report.message.take() {
                    Some(old) => format!("{notice} {old}"),
                    None => notice,
                });
                append_boot_message(&mut report);
                finish_boot(state, &report, Some(owner));
                return report;
            }
            Err(recovery) => {
                let message = format!(
                    "{error}\n原配置 {} 也未能重新启动：{recovery}。已有文件与可用恢复资料已保留，请选择配置或新建配置。",
                    previous.display()
                );
                let report = BootReport {
                    phase: "setup",
                    config_path: previous.display().to_string(),
                    state: adapter::unavailable_view(message.clone()),
                    message: Some(message),
                };
                finish_boot(state, &report, None);
                return report;
            }
        }
    }
    let report = BootReport {
        phase: "setup",
        config_path: attempted.display().to_string(),
        state: adapter::unavailable_view(error),
        message: Some(error.to_owned()),
    };
    finish_boot(state, &report, None);
    report
}

/// A completed transition always returns the actual resulting boot report,
/// including a recovered original configuration. Its message reports failure.
pub fn switch_config(
    app: &AppHandle,
    path: &Path,
    discard_unsaved: bool,
) -> Result<BootReport, String> {
    if !path.is_absolute() {
        return Err("请选择配置文件的完整绝对路径。".into());
    }
    load_config(path)?;
    let directory = bootstrap_directory(app)?;
    let state = app.state::<AppState>();
    let _operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut previous = take_for_transition(&state, discard_unsaved)?;
    if let Some(mut owner) = previous.owner.take() {
        if let Err(error) = owner.stop() {
            return Ok(recover_previous(
                &state,
                &directory,
                previous.path.as_deref(),
                path,
                &format!("原控制器停止未能确认：{error}"),
            ));
        }
    }
    match start_existing(&directory, path) {
        Ok((mut report, owner)) => {
            note_previous_registration(&mut report, &previous, path);
            finish_boot(&state, &report, Some(owner));
            Ok(report)
        }
        Err(error) => Ok(recover_previous(
            &state,
            &directory,
            previous.path.as_deref(),
            path,
            &format!("新配置 {} 无法启动：{error}", path.display()),
        )),
    }
}

pub fn import_settings(
    app: &AppHandle,
    input: &Path,
    destination: &Path,
    options: ImportOptions,
    discard_unsaved: bool,
) -> Result<SettingsTransition, String> {
    management::validate_import_options(&options)?;
    if !destination.is_absolute() {
        return Err("导入目标需要完整的绝对配置路径。".into());
    }
    if std::fs::canonicalize(input)
        .ok()
        .zip(std::fs::canonicalize(destination).ok())
        .is_some_and(|(source, target)| source == target)
    {
        return Err("请选择与设置包文件不同的配置目标。".into());
    }
    let directory = bootstrap_directory(app)?;
    let state = app.state::<AppState>();
    let _operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // Parsing and preview are cold work and fail before the current owner stops.
    let preview = management::preview_bundle(input, Some(destination), options.mode)?;
    if options
        .expected_source_revision
        .as_ref()
        .is_some_and(|r| r != &preview.source_revision)
        || options.expected_destination_revision != preview.destination_revision
    {
        return Err("设置包或目标配置在预览后发生了变化，请重新预览。".into());
    }
    let mut previous = take_for_transition(&state, discard_unsaved)?;
    if let Some(mut owner) = previous.owner.take() {
        if let Err(error) = owner.stop() {
            let error = format!("原控制器停止未能确认：{error}");
            let boot = recover_previous(
                &state,
                &directory,
                previous.path.as_deref(),
                destination,
                &error,
            );
            return Ok(SettingsTransition {
                boot,
                imported: None,
                error: Some(error),
            });
        }
    }
    let imported = match management::import_bundle(input, destination, options) {
        Ok(imported) => imported,
        Err(error) => {
            let boot = recover_previous(
                &state,
                &directory,
                previous.path.as_deref(),
                destination,
                &error,
            );
            return Ok(SettingsTransition {
                boot,
                imported: None,
                error: Some(error),
            });
        }
    };
    match start_existing(&directory, &imported.config_path) {
        Ok((mut report, owner)) => {
            note_previous_registration(&mut report, &previous, &imported.config_path);
            finish_boot(&state, &report, Some(owner));
            Ok(SettingsTransition {
                boot: report,
                imported: Some(imported),
                error: None,
            })
        }
        Err(start_error) => {
            let rollback = management::rollback_import(&imported);
            let error = match rollback {
                Ok(()) if imported.recovery_directory.is_some() => format!(
                    "导入后的配置无法启动：{start_error}。已恢复目标位置原有资料；新的导入资料仍保留在恢复目录。"
                ),
                Ok(()) => format!(
                    "导入后的配置无法启动：{start_error}。新配置仍保留在 {}，可修复后重新打开。",
                    imported.config_path.display()
                ),
                Err(error) => {
                    format!("导入后的配置无法启动：{start_error}\n自动恢复未完成：{error}")
                }
            };
            let boot = recover_previous(
                &state,
                &directory,
                previous.path.as_deref(),
                destination,
                &error,
            );
            Ok(SettingsTransition {
                boot,
                imported: Some(imported),
                error: Some(error),
            })
        }
    }
}

/// True grants the caller permission to run shutdown/app.exit after this
/// function releases operation. Cancellation restores ordinary control.
pub fn request_quit(app: &AppHandle) -> Result<bool, String> {
    let state = app.state::<AppState>();
    {
        let mut inner = state.inner.lock().expect("participant owner");
        if inner.exit_inflight {
            return Ok(false);
        }
        inner.exit_inflight = true;
    }
    let _operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let result = (|| -> Result<bool, String> {
        let (handle, path) = {
            let mut inner = state.inner.lock().expect("participant owner");
            while matches!(inner.boot, Boot::Starting | Boot::Stopping) {
                inner = state.ready.wait(inner).expect("participant transition");
            }
            if inner.shutdown_requested || matches!(inner.boot, Boot::Stopped) {
                return Ok(true);
            }
            (
                inner.owner.as_ref().map(RunningParticipant::handle),
                finished_path(&inner).unwrap_or_default(),
            )
        };
        let changes = if let Some(handle) = handle {
            match query_pending(&handle) {
                Ok(changes) => changes,
                Err(error) => {
                    let snapshot = handle.snapshot();
                    let mut changes = Vec::new();
                    if snapshot.unsaved {
                        changes.push(UnsavedChange {
                            kind: "configuration",
                            path: path.clone(),
                            detail: snapshot.persistence_error,
                        });
                    }
                    if let Some(session) = snapshot.room.session {
                        for (kind, status) in [
                            ("global", session.rule_files.global),
                            ("room", session.rule_files.room),
                        ] {
                            if status.outcome.unsaved() {
                                if let Some(path) = status.path {
                                    changes.push(UnsavedChange {
                                        kind,
                                        path,
                                        detail: status.outcome.detail().map(str::to_owned),
                                    });
                                }
                            }
                        }
                    }
                    changes.push(UnsavedChange {
                        kind: "unknown",
                        path,
                        detail: Some(format!("无法读取最新保存状态：{error}")),
                    });
                    changes
                }
            }
        } else {
            Vec::new()
        };
        if changes.is_empty() {
            return Ok(true);
        }
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
        crate::native_dialogs::confirm_discard(app, &pending_text(&changes))
    })();
    if !matches!(result, Ok(true)) {
        let mut inner = state.inner.lock().expect("participant owner");
        inner.exit_inflight = false;
        state.ready.notify_all();
    }
    result
}

pub fn shutdown(app: &AppHandle) {
    let running = {
        let state = app.state::<AppState>();
        let mut inner = state.inner.lock().expect("participant owner");
        inner.shutdown_requested = true;
        while matches!(inner.boot, Boot::Starting | Boot::Stopping) {
            inner = state.ready.wait(inner).expect("participant shutdown");
        }
        if matches!(inner.boot, Boot::Stopped) {
            return;
        }
        inner.boot = Boot::Stopping;
        inner.owner.take()
    };
    if let Some(mut running) = running {
        let _ = running.stop();
    }
    let state = app.state::<AppState>();
    let mut inner = state.inner.lock().expect("participant owner");
    inner.boot = Boot::Stopped;
    state.ready.notify_all();
}

fn stopped_report() -> BootReport {
    BootReport {
        phase: "setup",
        config_path: String::new(),
        state: adapter::unavailable_view("程序正在退出，参与控制器已停止。"),
        message: Some("程序正在退出。".into()),
    }
}

fn boot_from_disk(app: &AppHandle) -> (BootReport, Option<RunningParticipant>) {
    let selected = startup_config_path().and_then(|explicit| match explicit {
        Some(path) => Ok((path, true)),
        None => default_config_path(app).and_then(|default| {
            load_bootstrap_path(default.parent().expect("config directory"), &default)
        }),
    });
    let (path, pinned) = match selected {
        Ok(selected) => selected,
        Err(error) => {
            return (
                BootReport {
                    phase: "setup",
                    config_path: String::new(),
                    state: adapter::unavailable_view(error.clone()),
                    message: Some(error),
                },
                None,
            );
        }
    };
    if !pinned {
        match fs::metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (
                    BootReport {
                        phase: "setup",
                        config_path: path.display().to_string(),
                        state: adapter::unavailable_view(
                            "还没有本机配置。填写名称并创建配置后，才会保存身份和联系选择。",
                        ),
                        message: None,
                    },
                    None,
                );
            }
            _ => {}
        }
    }
    match start(&path) {
        Ok(running) => {
            let state = adapter::map_snapshot(&running.handle().snapshot());
            (
                BootReport {
                    phase: "running",
                    config_path: path.display().to_string(),
                    state,
                    message: None,
                },
                Some(running),
            )
        }
        Err(error) => (
            BootReport {
                phase: "setup",
                config_path: path.display().to_string(),
                state: adapter::unavailable_view(format!(
                    "现有配置无法启动，文件已原样保留。{error}"
                )),
                message: Some(error),
            },
            None,
        ),
    }
}

/// Explicit task arguments pin exactly the same configuration used at opt-in.
/// An invalid path never falls through to a different bootstrap configuration.
fn startup_config_path() -> Result<Option<PathBuf>, String> {
    let mut args = std::env::args_os().skip(1);
    let Some(flag) = args.next() else {
        return Ok(None);
    };
    if flag != "--config" {
        return Err("不支持的启动参数。可使用 --config 指定完整配置路径。".into());
    }
    let path = args
        .next()
        .map(PathBuf::from)
        .ok_or("--config 缺少配置路径。")?;
    if args.next().is_some() || !path.is_absolute() {
        return Err("--config 需要一个完整的绝对配置路径。".into());
    }
    if !path.is_file() {
        return Err(format!(
            "启动时指定的配置文件已移动或不存在：{}。请重新选择配置并重新启用登录自启。",
            path.display()
        ));
    }
    Ok(Some(path))
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BootstrapPath {
    config_path: PathBuf,
}

fn load_bootstrap_path(directory: &Path, default: &Path) -> Result<(PathBuf, bool), String> {
    let pointer = directory.join("bootstrap.json");
    let bytes = match fs::read(&pointer) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((default.to_path_buf(), false));
        }
        Err(error) => {
            return Err(format!(
                "无法读取配置路径记录 {}：{error}",
                pointer.display()
            ));
        }
    };
    let saved: BootstrapPath = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "配置路径记录 {} 无法解析，文件已保留：{error}",
            pointer.display()
        )
    })?;
    if !saved.config_path.is_absolute() {
        return Err(format!(
            "配置路径记录 {} 需要包含完整的绝对路径，文件已保留。",
            pointer.display()
        ));
    }
    Ok((saved.config_path, true))
}

fn save_bootstrap_path(directory: &Path, path: &Path) -> Result<(), String> {
    fs::create_dir_all(directory)
        .map_err(|error| format!("无法创建配置目录 {}：{error}", directory.display()))?;
    let bytes = serde_json::to_vec_pretty(&BootstrapPath {
        config_path: path.to_path_buf(),
    })
    .map_err(|error| format!("无法编码配置路径：{error}"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("无法建立配置路径临时文件名：{error}"))?
        .as_nanos();
    let stage = directory.join(format!(".bootstrap-{}-{stamp}.tmp", std::process::id()));
    let pointer = directory.join("bootstrap.json");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)
        .map_err(|error| format!("无法建立配置路径临时文件 {}：{error}", stage.display()))?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        // Both paths are in the same directory. std::fs::rename replaces the
        // destination atomically, including on Windows.
        fs::rename(&stage, &pointer)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&stage);
        return Err(format!(
            "无法保存配置路径记录 {}：{error}",
            pointer.display()
        ));
    }
    Ok(())
}

fn default_config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法确定本机配置目录：{error}"))?;
    Ok(directory.join("participant.json"))
}

fn parse_bind(value: &str) -> Result<SocketAddr, String> {
    value
        .parse()
        .map_err(|_| format!("绑定地址 {value} 不是有效的网络地址，例如 0.0.0.0:47000。"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_directory() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rlb-desktop-{}-{stamp}", std::process::id()))
    }

    fn request() -> SetupRequest {
        SetupRequest {
            name: "书房".into(),
            bind: "0.0.0.0:47000".into(),
            existing_path: None,
            new_path: None,
        }
    }

    #[test]
    fn rejects_invalid_setup_before_creating_configuration() {
        let directory = scratch_directory();
        let path = directory.join("participant.json");
        let mut bad_bind = request();
        bad_bind.bind = "not-an-address".into();
        assert!(prepare_setup(&path, bad_bind).is_err());
        let mut bad_name = request();
        bad_name.name = "\n".into();
        assert!(prepare_setup(&path, bad_name).is_err());
        assert!(!directory.exists());
        let prepared = prepare_setup(&path, request()).unwrap();
        assert!(prepared.new_config.is_some());
        assert!(!directory.exists());
    }

    #[test]
    fn local_setup_prepares_only_local_state_without_room_intent() {
        let directory = scratch_directory();
        let path = directory.join("participant.json");
        let prepared = prepare_setup(&path, request()).unwrap();
        let config = prepared.new_config.unwrap();
        assert!(!config.group.joined);
        assert!(config.contacts.is_empty());
        assert!(config.members.is_empty());
        assert!(!config.room.auto_enter);
        assert!(config.room.selected.is_none());
        assert!(!directory.exists());
    }

    #[test]
    fn existing_default_file_is_refused_and_bytes_stay() {
        let directory = scratch_directory();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("participant.json");
        let bytes = b"keep-these-bytes";
        fs::write(&path, bytes).unwrap();
        let error = match prepare_setup(&path, request()) {
            Err(error) => error,
            Ok(_) => panic!("existing file was accepted"),
        };
        assert!(error.contains("已经存在"), "{error}");
        assert!(error.contains("不会被覆盖"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn corrupt_existing_path_reports_and_keeps_bytes() {
        let directory = scratch_directory();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("broken.json");
        let bytes = b"{not-json";
        fs::write(&path, bytes).unwrap();
        let mut request = request();
        request.existing_path = Some(path.display().to_string());
        let error = match prepare_setup(&directory.join("participant.json"), request) {
            Err(error) => error,
            Ok(_) => panic!("corrupt file was accepted"),
        };
        assert!(error.contains("invalid"), "{error}");
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(!directory.join("participant.json").exists());
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn opening_existing_config_preserves_path_bytes_and_intent_during_preparation() {
        let directory = scratch_directory();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("participant.json");
        let mut config =
            ParticipantConfig::create("书房", "0.0.0.0:47000".parse().unwrap()).unwrap();
        config.room.auto_enter = true;
        config.room.selected = Some(rustlanbus_engine::participant::RoomStartup::Join {
            profile_path: directory.join("saved-room.json"),
        });
        create_config_file(&path, &config).unwrap();
        let before = fs::read(&path).unwrap();
        let mut request = request();
        request.name = "其他".into();
        request.bind = "127.0.0.1:1".into();
        request.existing_path = Some(path.display().to_string());
        let prepared = prepare_setup(&directory.join("unused.json"), request).unwrap();
        assert!(prepared.new_config.is_none());
        assert_eq!(prepared.path, path);
        assert_eq!(load_config(&prepared.path).unwrap().room, config.room);
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn setup_request_rejects_removed_fields() {
        for field in ["groupName", "remoteMemberId", "remoteAddress", "remoteName"] {
            let mut value = serde_json::json!({
                "name": "书房",
                "bind": "0.0.0.0:47000",
                "existingPath": null,
            });
            value[field] = serde_json::json!("x");
            let error = serde_json::from_value::<SetupRequest>(value).expect_err("old setup field");
            let text = error.to_string();
            assert!(text.contains("unknown field"), "{text}");
            assert!(text.contains(field), "{text}");
        }
        let parsed: SetupRequest = serde_json::from_value(serde_json::json!({
            "name": "书房",
            "bind": "0.0.0.0:47000",
            "existingPath": null,
        }))
        .unwrap();
        assert_eq!(parsed.name, "书房");
        assert!(parsed.existing_path.is_none());
    }

    #[test]
    fn setup_claim_prevents_duplicate_start_and_can_retry_a_failure() {
        let state = AppState::new();
        claim_setup(&state).unwrap();
        assert!(claim_setup(&state).is_err());
        let report = BootReport {
            phase: "setup",
            config_path: String::new(),
            state: adapter::unavailable_view("输入需要修正。"),
            message: None,
        };
        finish_boot(&state, &report, None);
        claim_setup(&state).unwrap();
        finish_boot(&state, &report, None);
        state.inner.lock().unwrap().shutdown_requested = true;
        assert!(claim_setup(&state).is_err());
    }

    #[test]
    fn bootstrap_path_replaces_atomically_and_preserves_corrupt_input() {
        let directory = scratch_directory();
        let default = directory.join("participant.json");
        assert_eq!(
            load_bootstrap_path(&directory, &default).unwrap(),
            (default.clone(), false)
        );
        let first = directory.join("first.json");
        let second = directory.join("second.json");
        save_bootstrap_path(&directory, &first).unwrap();
        save_bootstrap_path(&directory, &second).unwrap();
        assert_eq!(
            load_bootstrap_path(&directory, &default).unwrap(),
            (second, true)
        );
        let pointer = directory.join("bootstrap.json");
        fs::write(&pointer, b"{corrupt").unwrap();
        assert!(load_bootstrap_path(&directory, &default).is_err());
        assert_eq!(fs::read(&pointer).unwrap(), b"{corrupt");
        fs::remove_file(pointer).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
