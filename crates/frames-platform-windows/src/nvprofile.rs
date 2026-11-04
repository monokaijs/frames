//! Holds the NVIDIA GPU in its high-performance state for this program only,
//! through the driver's application profiles (NVAPI DRS), instead of keeping
//! the GPU busy with a dummy load.
//!
//! NVENC runs 2-2.5x slower when an otherwise idle GPU drops to a low power
//! state (spike 0-C). "Power management mode: Prefer maximum performance" for
//! this executable keeps the clocks up. The setting is applied before the
//! D3D11 device is created (the driver reads profiles when a device starts)
//! and restored exactly when the guard drops: the previous value, no setting,
//! or no profile at all if this guard created it.
//!
//! Layouts and interface ids come from NVIDIA's NVAPI headers
//! (github.com/NVIDIA/nvapi, MIT): `nvapi.h`, `nvapi_interface.h`,
//! `NvApiDriverSettings.h`.

use std::ffi::c_void;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCSTR, w};

const UNICODE_MAX: usize = 2048;
type Unicode = [u16; UNICODE_MAX];

// nvapi_interface.h
const ID_INITIALIZE: u32 = 0x0150_e828;
const ID_CREATE_SESSION: u32 = 0x0694_d52e;
const ID_DESTROY_SESSION: u32 = 0xdad9_cff8;
const ID_LOAD_SETTINGS: u32 = 0x375d_bd6b;
const ID_SAVE_SETTINGS: u32 = 0xfcbc_7e14;
const ID_CREATE_PROFILE: u32 = 0xcc17_6068;
const ID_DELETE_PROFILE: u32 = 0x1709_3206;
const ID_FIND_PROFILE_BY_NAME: u32 = 0x7e4a_9a0b;
const ID_CREATE_APPLICATION: u32 = 0x4347_a9de;
const ID_FIND_APPLICATION_BY_NAME: u32 = 0xeee5_66b2;
const ID_SET_SETTING: u32 = 0x577d_d202;
const ID_GET_SETTING: u32 = 0x73bf_8338;
const ID_DELETE_PROFILE_SETTING: u32 = 0xe4a2_6362;

// NvApiDriverSettings.h
const PREFERRED_PSTATE_ID: u32 = 0x1057_eb71;
const PREFERRED_PSTATE_PREFER_MAX: u32 = 1;

// nvapi.h / nvapi_lite_common.h
const NVAPI_OK: i32 = 0;
const NVDRS_DWORD_TYPE: u32 = 0;
const NVDRS_CURRENT_PROFILE_LOCATION: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct BinarySetting {
    length: u32,
    data: [u8; 4096],
}

/// The value union without the header's newer `NvU64` member: with it the
/// union aligns to 8 and the struct grows to 12,328 bytes, which drivers
/// built against the original layout (12,320) reject as an incompatible
/// version (-9, seen on driver 621.01). Frames only uses DWORD values.
#[repr(C)]
#[derive(Clone, Copy)]
union SettingValue {
    dword: u32,
    binary: BinarySetting,
    string: Unicode,
}

/// NVDRS_SETTING_V1
#[repr(C)]
struct Setting {
    version: u32,
    name: Unicode,
    id: u32,
    kind: u32,
    location: u32,
    is_current_predefined: u32,
    is_predefined_valid: u32,
    predefined: SettingValue,
    current: SettingValue,
}

/// NVDRS_PROFILE_V1
#[repr(C)]
struct Profile {
    version: u32,
    name: Unicode,
    gpu_support: u32,
    is_predefined: u32,
    applications: u32,
    settings: u32,
}

/// NVDRS_APPLICATION_V4
#[repr(C)]
struct Application {
    version: u32,
    is_predefined: u32,
    name: Unicode,
    friendly_name: Unicode,
    launcher: Unicode,
    file_in_folder: Unicode,
    flags: u32,
    command_line: Unicode,
}

const _: () = assert!(std::mem::size_of::<Setting>() == 12_320);
const _: () = assert!(std::mem::size_of::<Profile>() == 4_116);
const _: () = assert!(std::mem::size_of::<Application>() == 20_492);

const fn version<T>(number: u32) -> u32 {
    std::mem::size_of::<T>() as u32 | (number << 16)
}

fn zeroed<T>() -> Box<T> {
    // SAFETY: every field of these C structs is an integer or array of them.
    unsafe { Box::new(std::mem::zeroed()) }
}

fn unicode(text: &str) -> Box<Unicode> {
    let mut out: Box<Unicode> = zeroed();
    for (slot, unit) in out
        .iter_mut()
        .zip(text.encode_utf16().take(UNICODE_MAX - 1))
    {
        *slot = unit;
    }
    out
}

type Session = *mut c_void;
type ProfileHandle = *mut c_void;

struct Api {
    create_session: unsafe extern "C" fn(*mut Session) -> i32,
    destroy_session: unsafe extern "C" fn(Session) -> i32,
    load_settings: unsafe extern "C" fn(Session) -> i32,
    save_settings: unsafe extern "C" fn(Session) -> i32,
    create_profile: unsafe extern "C" fn(Session, *mut Profile, *mut ProfileHandle) -> i32,
    delete_profile: unsafe extern "C" fn(Session, ProfileHandle) -> i32,
    find_profile_by_name: unsafe extern "C" fn(Session, *const u16, *mut ProfileHandle) -> i32,
    create_application: unsafe extern "C" fn(Session, ProfileHandle, *mut Application) -> i32,
    find_application_by_name:
        unsafe extern "C" fn(Session, *const u16, *mut ProfileHandle, *mut Application) -> i32,
    set_setting: unsafe extern "C" fn(Session, ProfileHandle, *mut Setting) -> i32,
    get_setting: unsafe extern "C" fn(Session, ProfileHandle, u32, *mut Setting) -> i32,
    delete_profile_setting: unsafe extern "C" fn(Session, ProfileHandle, u32) -> i32,
}

/// Reinterprets an NVAPI function pointer as the function type it has.
///
/// # Safety
/// `F` must be the function's real signature.
unsafe fn cast<F: Copy>(pointer: *mut c_void) -> F {
    const { assert!(std::mem::size_of::<F>() == std::mem::size_of::<*mut c_void>()) };
    unsafe { std::mem::transmute_copy(&pointer) }
}

fn api() -> Result<Api, String> {
    unsafe {
        let module =
            LoadLibraryW(w!("nvapi64.dll")).map_err(|_| "no NVIDIA driver (nvapi64.dll)")?;
        let query = GetProcAddress(module, PCSTR(c"nvapi_QueryInterface".as_ptr().cast()))
            .ok_or("nvapi_QueryInterface missing")?;
        let query: unsafe extern "C" fn(u32) -> *mut c_void = std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "C" fn(u32) -> *mut c_void,
        >(query);
        let get = |id: u32| {
            let pointer = query(id);
            if pointer.is_null() {
                Err(format!("NVAPI function {id:#010x} unavailable"))
            } else {
                Ok(pointer)
            }
        };
        let initialize: unsafe extern "C" fn() -> i32 = cast(get(ID_INITIALIZE)?);
        let status = initialize();
        if status != NVAPI_OK {
            return Err(format!("NvAPI_Initialize failed ({status})"));
        }
        Ok(Api {
            create_session: cast(get(ID_CREATE_SESSION)?),
            destroy_session: cast(get(ID_DESTROY_SESSION)?),
            load_settings: cast(get(ID_LOAD_SETTINGS)?),
            save_settings: cast(get(ID_SAVE_SETTINGS)?),
            create_profile: cast(get(ID_CREATE_PROFILE)?),
            delete_profile: cast(get(ID_DELETE_PROFILE)?),
            find_profile_by_name: cast(get(ID_FIND_PROFILE_BY_NAME)?),
            create_application: cast(get(ID_CREATE_APPLICATION)?),
            find_application_by_name: cast(get(ID_FIND_APPLICATION_BY_NAME)?),
            set_setting: cast(get(ID_SET_SETTING)?),
            get_setting: cast(get(ID_GET_SETTING)?),
            delete_profile_setting: cast(get(ID_DELETE_PROFILE_SETTING)?),
        })
    }
}

fn check(call: &str, status: i32) -> Result<(), String> {
    if status == NVAPI_OK {
        Ok(())
    } else {
        Err(format!("{call} failed ({status})"))
    }
}

/// A loaded settings session, destroyed on drop.
struct Drs<'a> {
    api: &'a Api,
    session: Session,
}

impl<'a> Drs<'a> {
    fn open(api: &'a Api) -> Result<Self, String> {
        let mut session = std::ptr::null_mut();
        check("CreateSession", unsafe {
            (api.create_session)(&mut session)
        })?;
        let drs = Self { api, session };
        check("LoadSettings", unsafe { (api.load_settings)(session) })?;
        Ok(drs)
    }

    fn save(&self) -> Result<(), String> {
        check("SaveSettings", unsafe {
            (self.api.save_settings)(self.session)
        })
    }

    /// The profile that applies to `executable`, if any.
    fn application_profile(&self, executable: &str) -> Option<ProfileHandle> {
        let name = unicode(executable);
        let mut application: Box<Application> = zeroed();
        application.version = version::<Application>(4);
        let mut profile = std::ptr::null_mut();
        let status = unsafe {
            (self.api.find_application_by_name)(
                self.session,
                name.as_ptr(),
                &mut profile,
                &mut *application,
            )
        };
        (status == NVAPI_OK && !profile.is_null()).then_some(profile)
    }

    fn profile_by_name(&self, name: &str) -> Option<ProfileHandle> {
        let name = unicode(name);
        let mut profile = std::ptr::null_mut();
        let status =
            unsafe { (self.api.find_profile_by_name)(self.session, name.as_ptr(), &mut profile) };
        (status == NVAPI_OK && !profile.is_null()).then_some(profile)
    }

    fn create_profile(&self, name: &str, executable: &str) -> Result<ProfileHandle, String> {
        let mut info: Box<Profile> = zeroed();
        info.version = version::<Profile>(1);
        info.name = *unicode(name);
        let mut profile = std::ptr::null_mut();
        check("CreateProfile", unsafe {
            (self.api.create_profile)(self.session, &mut *info, &mut profile)
        })?;
        let mut application: Box<Application> = zeroed();
        application.version = version::<Application>(4);
        application.name = *unicode(executable);
        application.friendly_name = *unicode(name);
        check("CreateApplication", unsafe {
            (self.api.create_application)(self.session, profile, &mut *application)
        })?;
        Ok(profile)
    }

    /// This profile's own value of the power setting (not inherited).
    fn own_value(&self, profile: ProfileHandle) -> Option<u32> {
        let mut setting: Box<Setting> = zeroed();
        setting.version = version::<Setting>(1);
        let status = unsafe {
            (self.api.get_setting)(self.session, profile, PREFERRED_PSTATE_ID, &mut *setting)
        };
        // SAFETY: a DWORD setting's value is the `dword` member.
        let value = unsafe { setting.current.dword };
        (status == NVAPI_OK && setting.location == NVDRS_CURRENT_PROFILE_LOCATION).then_some(value)
    }

    fn set_value(&self, profile: ProfileHandle, value: u32) -> Result<(), String> {
        let mut setting: Box<Setting> = zeroed();
        setting.version = version::<Setting>(1);
        setting.id = PREFERRED_PSTATE_ID;
        setting.kind = NVDRS_DWORD_TYPE;
        setting.current.dword = value;
        check("SetSetting", unsafe {
            (self.api.set_setting)(self.session, profile, &mut *setting)
        })
    }
}

impl Drop for Drs<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.destroy_session)(self.session) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Restore {
    /// This guard created the profile: delete it.
    DeleteProfile,
    /// The profile had no own value: delete the setting.
    DeleteSetting,
    /// Put this value back.
    Value(u32),
}

/// While alive, the driver prefers maximum performance for this executable.
pub struct PreferMaxPerformance {
    api: Api,
    executable: String,
    profile_name: String,
    restore: Restore,
}

impl PreferMaxPerformance {
    /// Applies the setting for the running executable. Call before creating
    /// the D3D11 device. Fails harmlessly without an NVIDIA driver.
    pub fn apply() -> Result<Self, String> {
        let executable = std::env::current_exe()
            .ok()
            .and_then(|path| path.file_name().map(|n| n.to_string_lossy().to_lowercase()))
            .ok_or("cannot name the running executable")?;
        let profile_name = format!("Frames v2 ({executable})");
        let api = api()?;
        let restore = {
            let drs = Drs::open(&api)?;
            let (profile, restore) = match drs.application_profile(&executable) {
                Some(profile) => {
                    let restore = match drs.own_value(profile) {
                        Some(value) => Restore::Value(value),
                        None => Restore::DeleteSetting,
                    };
                    (profile, restore)
                }
                None => {
                    // A profile left by a crashed run: reuse and remove it.
                    let profile = match drs.profile_by_name(&profile_name) {
                        Some(profile) => profile,
                        None => drs.create_profile(&profile_name, &executable)?,
                    };
                    (profile, Restore::DeleteProfile)
                }
            };
            drs.set_value(profile, PREFERRED_PSTATE_PREFER_MAX)?;
            drs.save()?;
            restore
        };
        tracing::info!(%executable, ?restore, "GPU set to prefer maximum performance");
        Ok(Self {
            api,
            executable,
            profile_name,
            restore,
        })
    }

    fn undo(&self) -> Result<(), String> {
        let drs = Drs::open(&self.api)?;
        let profile = match self.restore {
            Restore::DeleteProfile => drs.profile_by_name(&self.profile_name),
            _ => drs.application_profile(&self.executable),
        }
        .ok_or("the profile disappeared")?;
        match self.restore {
            Restore::DeleteProfile => check("DeleteProfile", unsafe {
                (self.api.delete_profile)(drs.session, profile)
            })?,
            Restore::DeleteSetting => check("DeleteProfileSetting", unsafe {
                (self.api.delete_profile_setting)(drs.session, profile, PREFERRED_PSTATE_ID)
            })?,
            Restore::Value(value) => drs.set_value(profile, value)?,
        }
        drs.save()
    }
}

impl Drop for PreferMaxPerformance {
    fn drop(&mut self) {
        if let Err(error) = self.undo() {
            tracing::warn!(%error, "could not restore the GPU power setting");
        }
    }
}

/// Whether the driver currently prefers maximum performance for the running
/// executable (for checks; `None` without NVAPI or a profile).
pub fn prefers_max_performance() -> Option<bool> {
    let executable = std::env::current_exe()
        .ok()?
        .file_name()?
        .to_string_lossy()
        .to_lowercase();
    let api = api().ok()?;
    let drs = Drs::open(&api).ok()?;
    let profile = drs.application_profile(&executable)?;
    Some(drs.own_value(profile) == Some(PREFERRED_PSTATE_PREFER_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_is_applied_and_restored() {
        let before = prefers_max_performance();
        let guard = match PreferMaxPerformance::apply() {
            Ok(guard) => guard,
            Err(error) => {
                eprintln!("skipped: {error}");
                return;
            }
        };
        assert_eq!(prefers_max_performance(), Some(true));
        drop(guard);
        assert_eq!(prefers_max_performance(), before);
    }
}
