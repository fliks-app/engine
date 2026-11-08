//! Windows ML's execution providers: the catalog of the bundled `Microsoft.Windows.AI.MachineLearning`
//! library registers what Windows certified for the hardware into ONNX Runtime (spec section 1).
use ort::{device::Device, environment::Environment, memory::DeviceType};

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

/// Registers the certified providers Windows has for this machine; with `download`, fetches the
/// missing ones first (it can take minutes). Nothing on a Windows before 11 24H2 or without the
/// catalog library.
pub fn register(env: &Environment, download: bool) {
    #[cfg(windows)]
    imp::register(env, download);
    #[cfg(not(windows))]
    let _ = (env, download);
}

#[cfg(windows)]
mod imp {
    use ort::environment::Environment;
    use std::collections::HashSet;
    use std::ffi::{CStr, c_char, c_void};
    use std::sync::Mutex;

    /// Windows 11 24H2, the first build whose catalog serves providers.
    const MIN_BUILD: u32 = 26100;
    const CERTIFIED: i32 = 1;
    const NOT_PRESENT: i32 = 2;

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

    /// Providers already handed to ONNX Runtime; the library and catalog live as long as the process,
    /// since a registered provider's package stays in the process' dependency graph.
    static REGISTERED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

    pub fn register(env: &Environment, download: bool) {
        if build() < MIN_BUILD {
            tracing::debug!("Windows ML catalog needs build {MIN_BUILD}");
            return;
        }
        let Some(lib) = crate::karaoke::bundled("Microsoft.Windows.AI.MachineLearning.dll") else {
            tracing::debug!("no Windows ML catalog library in the bundle");
            return;
        };
        let mut done = REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = run(env, &lib, download, done.get_or_insert_with(HashSet::new)) {
            tracing::warn!("Windows ML execution providers: {e}");
        }
    }

    fn run(
        env: &Environment,
        lib: &str,
        download: bool,
        done: &mut HashSet<String>,
    ) -> Result<(), String> {
        // SAFETY: loads a Microsoft library from the bundle; it is never unloaded.
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
