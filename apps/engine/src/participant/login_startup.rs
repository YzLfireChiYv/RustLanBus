//! Explicit, current-user elevated logon startup. Observation never registers a task.

use std::path::Path;

use serde::Serialize;

use super::config::ParticipantConfig;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStartupState {
    pub requested: bool,
    /// None means registration could not be observed.
    pub registered: Option<bool>,
    pub task_name: Option<String>,
    pub executable: Option<String>,
    pub last_error: Option<String>,
}

pub fn inspect(path: &Path, config: &ParticipantConfig) -> LoginStartupState {
    operate(path, config, None)
}

pub(crate) fn set(path: &Path, config: &ParticipantConfig, enabled: bool) -> LoginStartupState {
    operate(path, config, Some(enabled))
}

fn operate(path: &Path, config: &ParticipantConfig, change: Option<bool>) -> LoginStartupState {
    let mut state = LoginStartupState {
        requested: config.login_startup,
        ..Default::default()
    };
    #[cfg(windows)]
    if let Err(error) = native::operate(path, config, change, &mut state) {
        state.last_error = Some(error);
    }
    #[cfg(not(windows))]
    {
        let _ = (path, change);
        state.last_error = Some("Login startup currently targets Windows.".into());
    }
    state
}

#[cfg(windows)]
mod native {
    use super::*;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::System::TaskScheduler::{
        IExecAction, ITaskService, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN,
        TASK_RUNLEVEL_HIGHEST, TaskScheduler,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::core::{BSTR, Interface};

    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub(super) fn operate(
        path: &Path,
        config: &ParticipantConfig,
        change: Option<bool>,
        state: &mut LoginStartupState,
    ) -> Result<(), String> {
        // Each call belongs to the cold preference owner; COM objects never
        // enter the audio owners or escape this initialized apartment.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(native_error)?;
        let _apartment = Apartment;
        let service: ITaskService =
            unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }
                .map_err(native_error)?;
        let empty = VARIANT::default();
        unsafe { service.Connect(&empty, &empty, &empty, &empty) }.map_err(native_error)?;
        let user = unsafe { service.ConnectedUser() }
            .map_err(native_error)?
            .to_string();
        let domain = unsafe { service.ConnectedDomain() }
            .map_err(native_error)?
            .to_string();
        let account = if domain.is_empty() {
            user
        } else {
            format!("{domain}\\{user}")
        };
        let account_hash = account
            .to_lowercase()
            .bytes()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        // The registration belongs to this configuration, independently of its
        // device identity (which a copy/import deliberately replaces).
        let path = std::fs::canonicalize(path).map_err(|error| {
            format!("The login-startup configuration path cannot be resolved: {error}")
        })?;
        let path_hash = path.to_string_lossy().to_lowercase().bytes().fold(
            0xcbf29ce484222325_u64,
            |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3),
        );
        let task_name = format!("RustLanBus-{account_hash:016x}-{path_hash:016x}");
        state.task_name = Some(task_name.clone());
        let folder = unsafe { service.GetFolder(&BSTR::from("\\")) }.map_err(native_error)?;
        let name = BSTR::from(task_name);
        if change == Some(false) {
            match unsafe { folder.DeleteTask(&name, 0) } {
                Ok(()) => {}
                Err(error) if missing(&error) => {}
                Err(error) => return Err(native_error(error)),
            }
        }
        let gui_arguments = format!("--config {}", quote_argument(&path.to_string_lossy()));
        let cli_arguments = format!(
            "participant run {}",
            quote_argument(&path.to_string_lossy())
        );
        if change == Some(true) {
            let executable = std::env::current_exe().map_err(|error| {
                format!("The running executable path cannot be resolved: {error}")
            })?;
            let arguments = if crate::participant::cli::is_cli_entry() {
                &cli_arguments
            } else {
                &gui_arguments
            };
            let directory = executable
                .parent()
                .ok_or("The running executable has no parent directory.")?;
            let xml = task_xml(
                &account,
                &executable.to_string_lossy(),
                arguments,
                &directory.to_string_lossy(),
            );
            unsafe {
                folder.RegisterTask(
                    &name,
                    &BSTR::from(xml),
                    TASK_CREATE_OR_UPDATE.0,
                    &VARIANT::from(account.as_str()),
                    &empty,
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &empty,
                )
            }
            .map_err(native_error)?;
        }
        let task = match unsafe { folder.GetTask(&name) } {
            Ok(task) => task,
            Err(error) if missing(&error) => {
                state.registered = Some(false);
                if config.login_startup {
                    state.last_error = Some("Login startup was requested, but its Windows task is missing. Enable it again to register the current executable and configuration.".into());
                }
                return Ok(());
            }
            Err(error) => return Err(native_error(error)),
        };
        state.registered = Some(false);
        let definition = unsafe { task.Definition() }.map_err(native_error)?;
        let actions = unsafe { definition.Actions() }.map_err(native_error)?;
        let mut count = 0;
        unsafe { actions.Count(&mut count) }.map_err(native_error)?;
        if count != 1 {
            return Err("The login-startup task has an unexpected action. Enable it again to replace this application's task.".into());
        }
        let action: IExecAction = unsafe { actions.get_Item(1) }
            .map_err(native_error)?
            .cast()
            .map_err(native_error)?;
        let mut executable = BSTR::new();
        let mut arguments = BSTR::new();
        unsafe { action.Path(&mut executable) }.map_err(native_error)?;
        unsafe { action.Arguments(&mut arguments) }.map_err(native_error)?;
        let executable = executable.to_string();
        state.executable = Some(executable.clone());
        let principal = unsafe { definition.Principal() }.map_err(native_error)?;
        let mut level = Default::default();
        unsafe { principal.RunLevel(&mut level) }.map_err(native_error)?;
        if !unsafe { task.Enabled() }.map_err(native_error)?.as_bool() {
            return Err("The Windows login-startup task is disabled. Enable login startup again to repair it.".into());
        }
        if level != TASK_RUNLEVEL_HIGHEST {
            return Err("The Windows login-startup task does not request administrator execution. Enable it again to repair it.".into());
        }
        if !Path::new(&executable).is_file() {
            return Err("The login-startup executable was moved or removed. Enable login startup from its new location.".into());
        }
        let current = std::env::current_exe().map_err(|error| format!("The running executable path cannot be resolved: {error}"))?;
        if !crate::participant::management::same_path(Path::new(&executable), &current) {
            return Err("Login startup still refers to another program location. Enable it again to use this running version.".into());
        }
        if arguments.to_string() != gui_arguments && arguments.to_string() != cli_arguments {
            return Err("The login-startup task refers to another configuration path. Enable it again to use the current configuration.".into());
        }
        state.registered = Some(true);
        if !config.login_startup {
            state.last_error = Some("A Windows login-startup task is registered while the saved preference is off. Turn it off to remove the task, or turn it on to save the preference.".into());
        }
        Ok(())
    }

    fn native_error(error: windows::core::Error) -> String {
        format!("Windows login startup could not be accessed: {error}")
    }

    fn missing(error: &windows::core::Error) -> bool {
        matches!(error.code().0 as u32, 0x80070002 | 0x80070003)
    }

    fn escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    // CommandLineToArgvW/CRT convention: double trailing backslashes and those
    // immediately before a quote; Task Scheduler stores arguments separately.
    fn quote_argument(value: &str) -> String {
        let mut result = String::from("\"");
        let mut slashes = 0;
        for ch in value.chars() {
            if ch == '\\' {
                slashes += 1;
                continue;
            }
            result.extend(std::iter::repeat_n(
                '\\',
                if ch == '"' { slashes * 2 + 1 } else { slashes },
            ));
            slashes = 0;
            result.push(ch);
        }
        result.extend(std::iter::repeat_n('\\', slashes * 2));
        result.push('"');
        result
    }

    fn task_xml(account: &str, executable: &str, arguments: &str, directory: &str) -> String {
        let account = escape(account);
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<RegistrationInfo><Description>RustLanBus current-user login startup</Description></RegistrationInfo>
<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{account}</UserId></LogonTrigger></Triggers>
<Principals><Principal id="CurrentUser"><UserId>{account}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>false</AllowHardTerminate><StartWhenAvailable>true</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><Enabled>true</Enabled><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
<Actions Context="CurrentUser"><Exec><Command>{}</Command><Arguments>{}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>
</Task>"#,
            escape(executable),
            escape(arguments),
            escape(directory)
        )
    }
}
