//! User-invoked Windows file dialogs and a WebView-independent startup error.
//!
//! Dialog COM objects live and are released on one short-lived STA thread.
//! The caller invokes this cold operation from a blocking task, without an
//! application-owner lock. Selecting a path never writes configuration data.

pub fn pick_file(app: &tauri::AppHandle, kind: &str) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        windows_dialog::pick(app, kind)
    }
    #[cfg(not(windows))]
    {
        let _ = (app, kind);
        Err("原生文件选择目前仅支持 Windows。".into())
    }
}

/// Only fixed documentation/settings destinations can leave the application.
/// The frontend invokes this after a user clicks a reference button.
pub fn open_reference(key: &str) -> Result<(), String> {
    let target = match key {
        "fxsound_project" => "https://github.com/fxsound2/fxsound-app",
        "fxsound_download" => "https://www.fxsound.com/download",
        "windows_audio_guide" => {
            "https://support.microsoft.com/en-us/windows/hardware/audio/fix-app-audio-not-working-while-system-sounds-work-in-windows"
        }
        "windows_volume_mixer" => "ms-settings:apps-volume",
        "windows_sound_settings" => "ms-settings:sound",
        "webview2_download" => {
            "https://developer.microsoft.com/en-us/microsoft-edge/webview2/#download-section"
        }
        _ => return Err("不支持的参考入口。".into()),
    };
    #[cfg(windows)]
    {
        windows_dialog::open_target(target)
    }
    #[cfg(not(windows))]
    {
        Err(format!(
            "请手动复制此地址到对应浏览器或 Windows 设置：{target}"
        ))
    }
}

pub fn show_startup_error(message: &str) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW,
        };
        use windows::core::{PCWSTR, w};
        let text = message
            .replace('\0', "�")
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // This remains available before Tauri or WebView2 has opened a window.
        let result = unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                w!("RustLanBus 启动失败"),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
            )
        };
        if result.0 == 0 {
            eprintln!("RustLanBus startup failed: {message}");
        }
    }
    #[cfg(not(windows))]
    eprintln!("RustLanBus startup failed: {message}");
}

pub fn confirm_discard(app: &tauri::AppHandle, message: &str) -> Result<bool, String> {
    #[cfg(windows)]
    {
        use tauri::Manager;
        use windows::Win32::Foundation::{GetLastError, HWND};
        use windows::Win32::UI::WindowsAndMessaging::{
            IDCANCEL, IDOK, MB_DEFBUTTON2, MB_ICONWARNING, MB_OKCANCEL, MessageBoxW,
        };
        use windows::core::{PCWSTR, w};
        let owner = app
            .get_webview_window("main")
            .and_then(|window| window.hwnd().ok())
            .map(|handle| HWND(handle.0));
        let text = format!(
            "以下修改尚未保存：\n\n{message}\n\n选择“确定”放弃这些未保存修改并退出。\n选择“取消”返回程序处理。"
        );
        let text = text
            .replace('\0', "�")
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let result = unsafe {
            MessageBoxW(
                owner,
                PCWSTR(text.as_ptr()),
                w!("RustLanBus — 未保存的修改"),
                MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2,
            )
        };
        if result == IDOK {
            Ok(true)
        } else if result == IDCANCEL {
            Ok(false)
        } else {
            Err(format!("无法显示退出确认：{}", unsafe { GetLastError() }.0))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (app, message);
        Err("未保存内容的原生退出确认目前仅支持 Windows。".into())
    }
}

#[cfg(windows)]
mod windows_dialog {
    use std::ffi::c_void;
    use std::path::Path;

    use tauri::Manager;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_NOREADONLYRETURN,
        FOS_NOTESTFILECREATE, FOS_PATHMUSTEXIST, FileOpenDialog, FileSaveDialog, IFileDialog,
        IFileOpenDialog, IFileSaveDialog, SIGDN_FILESYSPATH, ShellExecuteW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{Interface, PCWSTR, PWSTR, w};

    #[derive(Clone, Copy)]
    enum Kind {
        SettingsImport,
        SettingsExport,
        ExistingConfig,
        NewConfig,
    }

    impl Kind {
        fn parse(kind: &str) -> Result<Self, String> {
            match kind {
                "settings_import" => Ok(Self::SettingsImport),
                "settings_export" => Ok(Self::SettingsExport),
                "existing_config" => Ok(Self::ExistingConfig),
                "new_config" => Ok(Self::NewConfig),
                _ => Err("不支持的文件选择用途。".into()),
            }
        }
        fn saves(self) -> bool {
            matches!(self, Self::SettingsExport | Self::NewConfig)
        }
        fn title(self) -> PCWSTR {
            match self {
                Self::SettingsImport => w!("选择 RustLanBus 设置包"),
                Self::SettingsExport => w!("将当前设置导出到新文件"),
                Self::ExistingConfig => w!("打开已有本机配置"),
                Self::NewConfig => w!("选择新配置文件的位置"),
            }
        }
    }

    struct Apartment;
    impl Apartment {
        fn enter() -> Result<Self, String> {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
                .ok()
                .map_err(native_error)?;
            Ok(Self)
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    struct ShellString(PWSTR);
    impl Drop for ShellString {
        fn drop(&mut self) {
            unsafe { CoTaskMemFree(Some(self.0.0 as *const c_void)) };
        }
    }

    pub(super) fn pick(app: &tauri::AppHandle, kind: &str) -> Result<Option<String>, String> {
        let kind = Kind::parse(kind)?;
        // A raw owner value is copied across threads; no COM interface escapes
        // its apartment. The main window stays owned by Tauri throughout.
        let owner = app
            .get_webview_window("main")
            .and_then(|window| window.hwnd().ok())
            .map_or(0, |handle| handle.0 as usize);
        std::thread::Builder::new()
            .name("rlb-file-dialog".into())
            .spawn(move || pick_on_sta(kind, owner))
            .map_err(|error| format!("文件选择窗口无法启动：{error}"))?
            .join()
            .map_err(|_| "文件选择线程意外退出。".to_owned())?
    }

    pub(super) fn open_target(target: &'static str) -> Result<(), String> {
        std::thread::Builder::new()
            .name("rlb-open-reference".into())
            .spawn(move || {
                let _apartment = Apartment::enter()?;
                let target_wide = target.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
                let result = unsafe {
                    ShellExecuteW(
                        None,
                        w!("open"),
                        PCWSTR(target_wide.as_ptr()),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    )
                };
                let code = result.0 as isize;
                if code > 32 {
                    Ok(())
                } else {
                    Err(format!(
                        "Windows 无法打开参考入口（错误 {code}）。请手动打开：{target}"
                    ))
                }
            })
            .map_err(|error| format!("参考入口无法启动：{error}"))?
            .join()
            .map_err(|_| "参考入口线程意外退出。".to_owned())?
    }

    fn pick_on_sta(kind: Kind, owner: usize) -> Result<Option<String>, String> {
        let _apartment = Apartment::enter()?;
        let dialog: IFileDialog = if kind.saves() {
            let dialog: IFileSaveDialog =
                unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER) }
                    .map_err(native_error)?;
            dialog.cast().map_err(native_error)?
        } else {
            let dialog: IFileOpenDialog =
                unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
                    .map_err(native_error)?;
            dialog.cast().map_err(native_error)?
        };
        let mut options = FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR | FOS_PATHMUSTEXIST;
        if kind.saves() {
            options |= FOS_NOREADONLYRETURN | FOS_NOTESTFILECREATE;
        } else {
            options |= FOS_FILEMUSTEXIST;
        }
        let filter = [COMDLG_FILTERSPEC {
            pszName: w!("JSON 文件 (*.json)"),
            pszSpec: w!("*.json"),
        }];
        unsafe {
            dialog.SetOptions(options).map_err(native_error)?;
            dialog.SetFileTypes(&filter).map_err(native_error)?;
            dialog.SetFileTypeIndex(1).map_err(native_error)?;
            dialog
                .SetDefaultExtension(w!("json"))
                .map_err(native_error)?;
            dialog.SetTitle(kind.title()).map_err(native_error)?;
        }
        let name = match kind {
            Kind::SettingsExport => {
                let time = unsafe { GetLocalTime() };
                Some(format!(
                    "RustLanBus-settings-{:04}{:02}{:02}-{:02}{:02}{:02}.json",
                    time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
                ))
            }
            Kind::NewConfig => Some("participant.json".to_owned()),
            _ => None,
        };
        let name = name.map(|name| name.encode_utf16().chain(Some(0)).collect::<Vec<_>>());
        if let Some(name) = &name {
            unsafe {
                dialog
                    .SetFileName(PCWSTR(name.as_ptr()))
                    .map_err(native_error)?;
            }
        }
        let owner = (owner != 0).then_some(HWND(owner as *mut c_void));
        match unsafe { dialog.Show(owner) } {
            Ok(()) => {}
            Err(error) if error.code().0 as u32 == 0x8007_04c7 => return Ok(None),
            Err(error) => return Err(native_error(error)),
        }
        let item = unsafe { dialog.GetResult() }.map_err(native_error)?;
        let text =
            ShellString(unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(native_error)?);
        if text.0.0.is_null() {
            return Err("Windows 文件选择没有返回文件路径。".into());
        }
        let path = unsafe { text.0.to_string() }
            .map_err(|_| "所选文件路径无法转换为 Unicode 文本。".to_owned())?;
        if kind.saves() && Path::new(&path).exists() {
            return Err("请选择新的文件名；现有文件已保留。".into());
        }
        Ok(Some(path))
    }

    fn native_error(error: windows::core::Error) -> String {
        format!(
            "Windows 文件选择失败（0x{:08x}）：{error}",
            error.code().0 as u32
        )
    }
}
