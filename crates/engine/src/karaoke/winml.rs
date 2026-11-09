//! The Windows ML runtime of the machine (the Windows App SDK framework package, never shipped) and
//! its execution providers: the catalog registers what Windows certified for the hardware into
//! ONNX Runtime (spec section 1).
use fliks_engine_core::api::WindowsMl;
use ort::{device::Device, environment::Environment, memory::DeviceType};
use std::path::PathBuf;

const CPU: &str = "CPUExecutionProvider";
/// The runtime's own provider, used through `ort::ep::DirectML`.
const DML: &str = "DmlExecutionProvider";

/// The provider name of an ONNX Runtime EP name: `NvTensorRtRtxExecutionProvider` is `nvtensorrtrtx`.
pub fn short_name(ep: &str) -> String {
    ep.strip_suffix("ExecutionProvider")
        .unwrap_or(ep)
        .to_ascii_lowercase()
}

/// The device the plugin provider `name` runs on, a GPU before an NPU; none off Windows, where the
/// providers are not plugins.
pub fn device<'e>(env: &'e Environment, name: &str) -> Option<Device<'e>> {
    if !cfg!(windows) {
        return None;
    }
    env.devices()
        .filter(|d| {
            d.ep()
                .is_ok_and(|e| e != CPU && e != DML && short_name(e) == name)
        })
        .min_by_key(|d| match d.hardware_device().ty() {
            DeviceType::GPU => 0,
            DeviceType::NPU => 1,
            _ => 2,
        })
}

/// The Windows ML runtime the engine runs: the directory of its framework package, pinned for the life of
/// the process.
pub struct System {
    dir: PathBuf,
}

impl System {
    pub const ONNXRUNTIME: &str = "onnxruntime.dll";
    #[cfg(windows)]
    const CATALOG: &str = "Microsoft.Windows.AI.MachineLearning.dll";

    pub fn onnxruntime(&self) -> String {
        self.dir
            .join(Self::ONNXRUNTIME)
            .to_string_lossy()
            .into_owned()
    }
}

/// The runtime installed for this user and pinned in the process, the same on every call; none off
/// Windows 11 22H2 and later, or when it is not installed.
pub fn system() -> Option<&'static System> {
    #[cfg(windows)]
    {
        static SYSTEM: std::sync::OnceLock<Option<System>> = std::sync::OnceLock::new();
        SYSTEM.get_or_init(imp::locate).as_ref()
    }
    #[cfg(not(windows))]
    None
}

/// Where the engine stands with the runtime (`runtime.windowsMl`): `using` when it runs its library,
/// `overridden` when `ORT_DYLIB_PATH` names another. None where the runtime does not apply.
pub fn status(using: bool, overridden: bool) -> Option<WindowsMl> {
    #[cfg(windows)]
    {
        if overridden || !imp::dependency_api() {
            return None;
        }
        Some(if using {
            WindowsMl::Active
        } else if imp::installed() {
            WindowsMl::Restart
        } else {
            WindowsMl::Missing
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (using, overridden);
        None
    }
}

/// Registers the certified providers Windows has for this machine into `env`, which runs the system
/// runtime; with `download`, fetches the missing ones first (it can take minutes). Nothing before
/// Windows 11 24H2.
pub fn register(env: &Environment, download: bool) {
    #[cfg(windows)]
    imp::register(env, download);
    #[cfg(not(windows))]
    let _ = (env, download);
}

#[cfg(windows)]
mod imp {
    use super::System;
    use ort::environment::Environment;
    use std::collections::HashSet;
    use std::ffi::{CStr, OsString, c_char, c_void};
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use windows_sys::Win32::Storage::Packaging::Appx::{
        FindPackagesByPackageFamily, GetPackagePathByFullName, PACKAGE_FILTER_HEAD,
    };

    /// The operating system's dynamic dependency API starts at Windows 11 22H2.
    const MIN_DEPENDENCY_BUILD: u32 = 22621;
    /// Windows 11 24H2, the first build whose catalog serves providers.
    const MIN_CATALOG_BUILD: u32 = 26100;
    /// Framework packages that carry the runtime, newest first.
    const FAMILIES: [&str; 2] = [
        "Microsoft.WindowsAppRuntime.2_8wekyb3d8bbwe",
        "Microsoft.WindowsAppRuntime.1.8_8wekyb3d8bbwe",
    ];
    const CERTIFIED: i32 = 1;
    const NOT_PRESENT: i32 = 2;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

    #[repr(C)]
    struct Info {
        name: *const c_char,
        version: *const c_char,
        package_family_name: *const c_char,
        library_path: *const c_char,
        package_root_path: *const c_char,
        ready_state: i32,
        certification: i32,
    }

    type Hr = i32;
    type EnumCb = unsafe extern "system" fn(*mut c_void, *const Info, *mut c_void) -> i32;
    type Create = unsafe extern "system" fn(*mut *mut c_void) -> Hr;
    type Enum = unsafe extern "system" fn(*mut c_void, EnumCb, *mut c_void) -> Hr;
    type Ready = unsafe extern "system" fn(*mut c_void) -> Hr;
    type PathSize = unsafe extern "system" fn(*mut c_void, *mut usize) -> Hr;
    type Path = unsafe extern "system" fn(*mut c_void, usize, *mut c_char, *mut usize) -> Hr;
    type TryCreate = unsafe extern "system" fn(
        *mut c_void,
        *const u16,
        u64,
        i32,
        i32,
        *const u16,
        i32,
        *mut *mut u16,
    ) -> Hr;
    type Add =
        unsafe extern "system" fn(*const u16, i32, i32, *mut *mut c_void, *mut *mut u16) -> Hr;

    struct Candidate {
        handle: *mut c_void,
        name: String,
        ready_state: i32,
    }

    unsafe extern "system" fn collect(ep: *mut c_void, info: *const Info, ctx: *mut c_void) -> i32 {
        // SAFETY: `ctx` is the `Vec` `enumerate` passes; `info` is valid for the call.
        let (found, info) = unsafe { (&mut *ctx.cast::<Vec<Candidate>>(), info.as_ref()) };
        if let Some(i) = info
            && !i.name.is_null()
            && i.certification == CERTIFIED
        {
            found.push(Candidate {
                handle: ep,
                name: unsafe { CStr::from_ptr(i.name) }
                    .to_string_lossy()
                    .into_owned(),
                ready_state: i.ready_state,
            });
        }
        1
    }

    /// The build number of this Windows, 0 when unknown.
    fn build() -> u32 {
        use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
        let mut v: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
        v.dwOSVersionInfoSize = size_of::<OSVERSIONINFOW>() as u32;
        // SAFETY: `v` is initialised and sized.
        if unsafe { RtlGetVersion(&mut v) } == 0 {
            v.dwBuildNumber
        } else {
            0
        }
    }

    pub fn dependency_api() -> bool {
        build() >= MIN_DEPENDENCY_BUILD
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Whether a framework package of the runtime is registered for this user.
    pub fn installed() -> bool {
        FAMILIES.iter().any(|f| {
            let (mut count, mut len) = (0u32, 0u32);
            // SAFETY: a size query: the family is NUL-terminated, the buffers are null with length 0.
            let rc = unsafe {
                FindPackagesByPackageFamily(
                    wide(f).as_ptr(),
                    PACKAGE_FILTER_HEAD,
                    &mut count,
                    std::ptr::null_mut(),
                    &mut len,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            rc == ERROR_INSUFFICIENT_BUFFER || (rc == 0 && count > 0)
        })
    }

    /// Reads and frees a string the dependency API allocated on the process heap.
    fn take(p: *mut u16) -> Option<String> {
        use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapFree};
        if p.is_null() {
            return None;
        }
        // SAFETY: `p` is a NUL-terminated string from the process heap, freed once here.
        unsafe {
            let n = (0..).take_while(|i| *p.add(*i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            HeapFree(GetProcessHeap(), 0, p.cast());
            Some(s)
        }
    }

    /// Pins the newest package of `family` for the process (the OS then does not service it) and
    /// gives its directory.
    fn pin(family: &str) -> Result<PathBuf, String> {
        // SAFETY: `kernelbase.dll` is in every process; the signatures are appmodel.h's.
        let kb =
            unsafe { libloading::Library::new("kernelbase.dll") }.map_err(|e| e.to_string())?;
        let create = *unsafe { kb.get::<TryCreate>(b"TryCreatePackageDependency\0") }
            .map_err(|e| e.to_string())?;
        let add =
            *unsafe { kb.get::<Add>(b"AddPackageDependency\0") }.map_err(|e| e.to_string())?;
        // Lifetime kind 0 is the process; the architecture is this build's (x64 4, arm64 16).
        let arch = if cfg!(target_arch = "aarch64") { 16 } else { 4 };
        let mut id = std::ptr::null_mut();
        let hr = unsafe {
            create(
                std::ptr::null_mut(),
                wide(family).as_ptr(),
                0,
                arch,
                0,
                std::ptr::null(),
                0,
                &mut id,
            )
        };
        if hr < 0 {
            return Err(format!("TryCreatePackageDependency 0x{hr:08x}"));
        }
        let id = take(id).ok_or("no dependency id")?;
        let (mut context, mut full) = (std::ptr::null_mut(), std::ptr::null_mut());
        let hr = unsafe { add(wide(&id).as_ptr(), 0, 0, &mut context, &mut full) };
        if hr < 0 {
            return Err(format!("AddPackageDependency 0x{hr:08x}"));
        }
        let full = wide(&take(full).ok_or("no package name")?);
        let mut len = 0u32;
        // SAFETY: the two-call size protocol of GetPackagePathByFullName.
        unsafe { GetPackagePathByFullName(full.as_ptr(), &mut len, std::ptr::null_mut()) };
        let mut buf = vec![0u16; len as usize];
        if unsafe { GetPackagePathByFullName(full.as_ptr(), &mut len, buf.as_mut_ptr()) } != 0 {
            return Err("no package path".into());
        }
        buf.truncate(len.saturating_sub(1) as usize);
        Ok(PathBuf::from(OsString::from_wide(&buf)))
    }

    /// The newest installed runtime that holds both libraries.
    pub fn locate() -> Option<System> {
        if !dependency_api() {
            return None;
        }
        for f in FAMILIES {
            match pin(f) {
                Ok(dir)
                    if dir.join(System::ONNXRUNTIME).is_file()
                        && dir.join(System::CATALOG).is_file() =>
                {
                    tracing::info!("Windows ML runtime at {}", dir.display());
                    return Some(System { dir });
                }
                Ok(dir) => tracing::debug!("{} is no Windows ML runtime", dir.display()),
                Err(e) => tracing::debug!("{f}: {e}"),
            }
        }
        tracing::info!("no Windows ML runtime installed for this user");
        None
    }

    /// Providers already handed to ONNX Runtime; the library and catalog live as long as the process,
    /// since a registered provider's package stays in the process' dependency graph.
    static REGISTERED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

    pub fn register(env: &Environment, download: bool) {
        if build() < MIN_CATALOG_BUILD {
            tracing::debug!("Windows ML catalog needs build {MIN_CATALOG_BUILD}");
            return;
        }
        let Some(lib) = super::system().map(|s| s.dir.join(System::CATALOG)) else {
            return;
        };
        let mut done = REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = run(env, &lib, download, done.get_or_insert_with(HashSet::new)) {
            tracing::warn!("Windows ML execution providers: {e}");
        }
    }

    fn run(
        env: &Environment,
        lib: &std::path::Path,
        download: bool,
        done: &mut HashSet<String>,
    ) -> Result<(), String> {
        // SAFETY: loads the system runtime's own library; it is never unloaded.
        let lib: &'static libloading::Library = Box::leak(Box::new(
            unsafe { libloading::Library::new(lib) }.map_err(|e| e.to_string())?,
        ));
        macro_rules! sym {
            ($t:ty, $n:literal) => {
                *unsafe { lib.get::<$t>($n) }.map_err(|e| e.to_string())?
            };
        }
        let create = sym!(Create, b"WinMLEpCatalogCreate\0");
        let enumerate = sym!(Enum, b"WinMLEpCatalogEnumProviders\0");
        let ensure = sym!(Ready, b"WinMLEpEnsureReady\0");
        let path_size = sym!(PathSize, b"WinMLEpGetLibraryPathSize\0");
        let path_of = sym!(Path, b"WinMLEpGetLibraryPath\0");
        let mut catalog = std::ptr::null_mut();
        let hr = unsafe { create(&mut catalog) };
        if hr < 0 {
            return Err(format!("WinMLEpCatalogCreate 0x{hr:08x}"));
        }
        let mut found: Vec<Candidate> = vec![];
        let hr = unsafe { enumerate(catalog, collect, (&raw mut found).cast()) };
        if hr < 0 {
            return Err(format!("WinMLEpCatalogEnumProviders 0x{hr:08x}"));
        }
        for c in found {
            if done.contains(&c.name) || (c.ready_state == NOT_PRESENT && !download) {
                continue;
            }
            let hr = unsafe { ensure(c.handle) };
            if hr < 0 {
                tracing::warn!(
                    provider = c.name.as_str(),
                    "Windows ML cannot ready it: 0x{hr:08x}"
                );
                continue;
            }
            let mut size = 0usize;
            if unsafe { path_size(c.handle, &mut size) } < 0 || size == 0 {
                continue;
            }
            let mut buf = vec![0 as c_char; size];
            if unsafe { path_of(c.handle, size, buf.as_mut_ptr(), std::ptr::null_mut()) } < 0 {
                continue;
            }
            let path = unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            match env.register_ep_library(c.name.as_str(), &path) {
                Ok(()) => {
                    tracing::info!(provider = c.name.as_str(), "Windows ML provider registered");
                    done.insert(c.name);
                }
                Err(e) => tracing::warn!(provider = c.name.as_str(), "cannot register {path}: {e}"),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::short_name;

    #[test]
    fn a_provider_is_its_runtime_name_without_the_suffix() {
        assert_eq!(
            short_name("NvTensorRtRtxExecutionProvider"),
            "nvtensorrtrtx"
        );
        assert_eq!(short_name("AMDGPUExecutionProvider"), "amdgpu");
        assert_eq!(short_name("MIGraphXExecutionProvider"), "migraphx");
        assert_eq!(short_name("OpenVINOExecutionProvider"), "openvino");
    }
}
