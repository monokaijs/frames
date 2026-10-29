#![cfg(windows)]
//! Optional Windows service broker. Installation is a separate, explicit CLI action.
use anyhow::Context;
use std::{ffi::OsString, path::PathBuf, time::Duration};
use windows::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{RemoteDesktop::WTSGetActiveConsoleSessionId, Threading::*},
    },
    core::*,
};
use windows_service::{
    define_windows_service,
    service::*,
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::*,
};
const NAME: &str = "Frames";
fn config() -> PathBuf {
    PathBuf::from(std::env::var_os("PROGRAMDATA").unwrap_or_else(|| "C:\\ProgramData".into()))
        .join("Frames")
}
fn installation() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into()))
        .join("Frames")
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub fn install() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("run service install from an Administrator terminal")?;
    let store = frames_core::trust::TrustStore::load()?;
    anyhow::ensure!(
        store
            .peers
            .values()
            .any(|p| p.unattended && p.allow_control),
        "pair at least one device using host --pair --unattended before installing the service"
    );
    let dir = installation();
    let data = config();
    anyhow::ensure!(
        !dir.exists() && !data.exists(),
        "installation directories already exist; inspect them before reinstalling (Frames will not overwrite an existing service installation)"
    );
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(&data)?;
    // Machine-scoped DPAPI is safe only with a protected key directory. Use SIDs,
    // not translated account names, and fail installation if ACL hardening fails.
    for path in [&dir, &data] {
        let status = std::process::Command::new("icacls.exe")
            .arg(path)
            .args([
                "/setowner",
                "*S-1-5-32-544",
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:(OI)(CI)F",
                "*S-1-5-32-544:(OI)(CI)F",
            ])
            .status()?;
        anyhow::ensure!(
            status.success(),
            "could not secure service installation directory"
        );
    }
    let current = std::env::current_exe()?;
    let executable = dir.join("frames.exe");
    if current != executable {
        std::fs::copy(&current, &executable)?;
    }
    let ffmpeg = std::env::var_os("FRAMES_FFMPEG")
        .map(PathBuf::from)
        .or_else(|| {
            std::process::Command::new("where.exe")
                .arg("ffmpeg.exe")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.lines().next().map(PathBuf::from))
        })
        .context("FFmpeg required for the service")?;
    if ffmpeg != dir.join("ffmpeg.exe") {
        std::fs::copy(ffmpeg, dir.join("ffmpeg.exe"))?;
    }
    frames_core::trust::Identity::load()?.install_service_identity(&data)?;
    let info = ServiceInfo {
        name: NAME.into(),
        display_name: "Frames Remote Windows".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: executable,
        launch_arguments: vec!["service".into(), "run".into()],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let service =
        manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    service.set_description(
        "Frames authenticated remote-window service and interactive-session broker",
    )?;
    service.start::<&str>(&[])?;
    println!("Frames service installed. Trust store: {}", data.display());
    Ok(())
}
pub fn uninstall() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    )?;
    let _ = service.stop();
    service.delete()?;
    println!("Frames service removed; binaries and trust records retained.");
    Ok(())
}
pub fn run() -> anyhow::Result<()> {
    service_dispatcher::start(NAME, ffi_main)?;
    Ok(())
}
define_windows_service!(ffi_main, service_main);
fn service_main(_: Vec<OsString>) {
    if let Err(error) = broker() {
        let _ = std::fs::write(config().join("service-error.log"), format!("{error:#}"));
    }
}
fn status(state: ServiceState) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    }
}
fn broker() -> anyhow::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = service_control_handler::register(NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    handle.set_service_status(status(ServiceState::Running))?;
    let mut process = None::<Agent>;
    let mut session = u32::MAX;
    loop {
        if rx.recv_timeout(Duration::from_secs(1)).is_ok() {
            break;
        }
        let active = unsafe { WTSGetActiveConsoleSessionId() };
        let exited = process
            .as_ref()
            .is_some_and(|p| unsafe { WaitForSingleObject(p.0, 0) == WAIT_OBJECT_0 });
        if active != session || exited || process.is_none() {
            process.take();
            session = active;
            if active != u32::MAX {
                match spawn_agent(active) {
                    Ok(agent) => process = Some(agent),
                    Err(error) => {
                        let _ = std::fs::write(
                            config().join("service-error.log"),
                            format!("{error:#}"),
                        );
                    }
                }
            }
        }
    }
    handle.set_service_status(status(ServiceState::StopPending))?;
    drop(process);
    handle.set_service_status(status(ServiceState::Stopped))?;
    Ok(())
}
struct Agent(HANDLE);
impl Drop for Agent {
    fn drop(&mut self) {
        unsafe {
            let _ = TerminateProcess(self.0, 0);
            let _ = WaitForSingleObject(self.0, 5000);
            let _ = CloseHandle(self.0);
        }
    }
}
fn spawn_agent(session: u32) -> anyhow::Result<Agent> {
    unsafe {
        let mut original = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
            &mut original,
        )?;
        let mut token = HANDLE::default();
        let result = DuplicateTokenEx(
            original,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        );
        let _ = CloseHandle(original);
        result?;
        let result = (|| -> anyhow::Result<Agent> {
            SetTokenInformation(
                token,
                TokenSessionId,
                &session as *const _ as *const _,
                std::mem::size_of::<u32>() as u32,
            )?;
            let executable = installation().join("frames.exe");
            let application = wide(&executable.to_string_lossy());
            let mut arguments = wide(&format!("\"{}\" service agent", executable.display()));
            let mut desktop = wide("winsta0\\default");
            let startup = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                ..Default::default()
            };
            let mut process = PROCESS_INFORMATION::default();
            let directory = wide(&installation().to_string_lossy());
            CreateProcessAsUserW(
                Some(token),
                PCWSTR(application.as_ptr()),
                Some(PWSTR(arguments.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR(directory.as_ptr()),
                &startup,
                &mut process,
            )?;
            let _ = CloseHandle(process.hThread);
            Ok(Agent(process.hProcess))
        })();
        let _ = CloseHandle(token);
        result
    }
}
pub fn agent_environment() -> anyhow::Result<()> {
    anyhow::ensure!(is_system()?, "service agent can only run as LocalSystem");
    // Called during process startup, before constructing the Tokio runtime.
    unsafe {
        std::env::set_var("FRAMES_CONFIG_DIR", config());
        std::env::set_var("FRAMES_PRIVILEGED_AGENT", "1");
        std::env::set_var("FRAMES_FFMPEG", installation().join("ffmpeg.exe"));
    }
    Ok(())
}
fn is_system() -> anyhow::Result<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let mut bytes = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut bytes);
        let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
        let result = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        );
        let _ = CloseHandle(token);
        result?;
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        Ok(IsWellKnownSid(user.User.Sid, WinLocalSystemSid).as_bool())
    }
}
