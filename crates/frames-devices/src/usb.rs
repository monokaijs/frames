//! Native Windows USB/IP tooling. Device leases run elevated and revoke on EOF.
use crate::protocol::DeviceInfo;
use std::net::TcpStream;
#[cfg(windows)]
use std::{
    io::{BufRead, Read},
    net::{Ipv4Addr, TcpListener},
    time::{Duration, Instant},
};

#[cfg(windows)]
const SCRIPT: &str = include_str!("../../../scripts/devices/usb.ps1");

#[cfg(windows)]
pub fn system_tool(relative: &str) -> anyhow::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }
    let mut buffer = [0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    anyhow::ensure!(
        length > 0 && length < buffer.len(),
        "Cannot locate Windows system tools"
    );
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length])).join(relative))
}
#[cfg(windows)]
fn powershell(
    action: &str,
    bus: &str,
    port: u16,
    token: &str,
) -> anyhow::Result<std::process::Command> {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new(system_tool("WindowsPowerShell/v1.0/powershell.exe")?);
    // Every interpolated value comes from a fixed action or a validated numeric ID/token.
    let script = format!(
        "& {{\n{SCRIPT}\n}} -Action '{action}' -BusId '{bus}' -Port {port} -Token '{token}'"
    );
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x08000000);
    Ok(cmd)
}
#[cfg(windows)]
fn run(action: &str, bus: &str, port: u16, token: &str) -> anyhow::Result<String> {
    let output = powershell(action, bus, port, token)?.output()?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(2000)
            .collect::<String>()
    );
    Ok(String::from_utf8(output.stdout)?
        .trim_start_matches('\u{feff}')
        .trim()
        .to_string())
}
pub fn inventory() -> anyhow::Result<Vec<DeviceInfo>> {
    #[cfg(windows)]
    {
        let text = run("list", "", 0, "")?;
        Ok(serde_json::from_str(&text)?)
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("USB forwarding is available between Windows computers in this release.")
    }
}
pub fn host_available() -> Result<(), String> {
    #[cfg(windows)]
    {
        run("host-check", "", 0, "")
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Err("USB receiving requires a Windows host in this release.".into())
    }
}
pub fn configure_isolation() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        run("isolate", "", 0, "")?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("USB setup requires Windows.")
    }
}
pub fn package_installed(target: &str) -> anyhow::Result<bool> {
    anyhow::ensure!(
        ["gamepad", "usb-export", "usb-host"].contains(&target),
        "Unknown driver package"
    );
    #[cfg(windows)]
    {
        Ok(run("installed", target, 0, "")?.eq_ignore_ascii_case("true"))
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}
pub struct ExportLease {
    _socket: TcpStream,
}
pub fn prepare_export(bus: &str) -> anyhow::Result<ExportLease> {
    anyhow::ensure!(crate::protocol::valid_bus_id(bus), "Invalid USB device ID");
    #[cfg(not(windows))]
    {
        anyhow::bail!("USB forwarding requires Windows.")
    }
    #[cfg(windows)]
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let token = uuid_token();
        let args = format!(
            "device-lease --bus-id {bus} --port {} --token {token}",
            listener.local_addr()?.port()
        );
        elevate(&args)?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    socket.set_nonblocking(false)?;
                    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut reader = std::io::BufReader::new(&socket).take(4096);
                    let mut line = String::new();
                    reader.read_line(&mut line)?;
                    let response: serde_json::Value = serde_json::from_str(&line)?;
                    if response["token"].as_str() != Some(&token) {
                        continue;
                    }
                    anyhow::ensure!(
                        response["ok"] == true,
                        "{}",
                        response["error"]
                            .as_str()
                            .unwrap_or("Cannot claim USB device")
                    );
                    socket.set_read_timeout(None)?;
                    return Ok(ExportLease { _socket: socket });
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    anyhow::bail!("USB setup timed out or administrator authorization was declined")
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}
#[cfg(windows)]
fn uuid_token() -> String {
    // OS CSPRNG, reused from the already loaded identity machinery.
    use windows::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let mut bytes = [0u8; 32];
    unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.unwrap();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub fn lease_worker(bus: &str, port: u16, token: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        crate::protocol::valid_bus_id(bus)
            && port >= 1024
            && token.len() == 64
            && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid USB lease arguments"
    );
    #[cfg(windows)]
    {
        run("lease", bus, port, token)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("USB forwarding requires Windows.")
    }
}
pub fn elevate(arguments: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_HIDE},
            core::PCWSTR,
        };
        let file: Vec<u16> = std::env::current_exe()?
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let args: Vec<u16> = arguments.encode_utf16().chain(Some(0)).collect();
        let verb: Vec<u16> = "runas\0".encode_utf16().collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(file.as_ptr()),
                PCWSTR(args.as_ptr()),
                None,
                SW_HIDE,
            )
        };
        anyhow::ensure!(
            result.0 as isize > 32,
            "Administrator setup was declined or could not start"
        );
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = arguments;
        anyhow::bail!("Driver setup is available on Windows.")
    }
}
#[cfg(windows)]
fn importer() -> anyhow::Result<std::path::PathBuf> {
    let path = std::path::PathBuf::from(
        std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into()),
    )
    .join("USBip/usbip.exe");
    anyhow::ensure!(
        path.is_file(),
        "Install USB/IP receiving support using Frames devices setup usb-host"
    );
    Ok(path)
}
pub fn attach(bus: &str, port: u16) -> anyhow::Result<u16> {
    anyhow::ensure!(
        crate::protocol::valid_bus_id(bus) && port >= 1024,
        "Invalid USB attachment"
    );
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new(importer()?)
            .args([
                "--tcp-port",
                &port.to_string(),
                "attach",
                "--remote",
                "127.0.0.1",
                "--bus-id",
                bus,
                "--once",
                "--terse",
                "--receive-mode",
                "low-latency",
            ])
            .creation_flags(0x08000000)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "USB attach failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let port = String::from_utf8(output.stdout)?.trim().parse::<u16>()?;
        anyhow::ensure!((1..=255).contains(&port), "Invalid imported USB port");
        Ok(port)
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("USB receiving requires Windows.")
    }
}
pub fn detach(port: u16) -> anyhow::Result<()> {
    anyhow::ensure!((1..=255).contains(&port), "Invalid imported USB port");
    #[cfg(windows)]
    {
        run("detach", "", port, "")?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("USB receiving requires Windows.")
    }
}
