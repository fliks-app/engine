//! Whether a real Vulkan device is usable, asked of the loader the recogniser would use.
//! Spec: `docs/spec/karaoke.md` section 4, rule 9.

/// True when the loader lists a device that is not a software rasteriser.
#[cfg(target_os = "linux")]
pub fn real_device() -> bool {
    // SAFETY: plain Vulkan 1.0 entry points of the system loader, called as the spec says.
    unsafe { probe() }.unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
pub fn real_device() -> bool {
    false
}

#[cfg(target_os = "linux")]
unsafe fn probe() -> Result<bool, libloading::Error> {
    use std::ffi::c_void;
    use std::ptr::null;

    #[repr(C)]
    struct InstanceInfo {
        s_type: u32,
        next: *const c_void,
        flags: u32,
        app: *const c_void,
        layers: u32,
        layer_names: *const c_void,
        extensions: u32,
        extension_names: *const c_void,
    }
    type Create = unsafe extern "C" fn(*const InstanceInfo, *const c_void, *mut *mut c_void) -> i32;
    type Destroy = unsafe extern "C" fn(*mut c_void, *const c_void);
    type Enumerate = unsafe extern "C" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
    type Props = unsafe extern "C" fn(*mut c_void, *mut u64);
    const TYPE_CPU: u32 = 4;
    const PROPS_U64: usize = 128;

    unsafe {
        let lib = libloading::Library::new("libvulkan.so.1")?;
        let create: libloading::Symbol<Create> = lib.get(b"vkCreateInstance\0")?;
        let destroy: libloading::Symbol<Destroy> = lib.get(b"vkDestroyInstance\0")?;
        let enumerate: libloading::Symbol<Enumerate> = lib.get(b"vkEnumeratePhysicalDevices\0")?;
        let props: libloading::Symbol<Props> = lib.get(b"vkGetPhysicalDeviceProperties\0")?;
        let info = InstanceInfo {
            s_type: 1,
            next: null(),
            flags: 0,
            app: null(),
            layers: 0,
            layer_names: null(),
            extensions: 0,
            extension_names: null(),
        };
        let mut instance = std::ptr::null_mut();
        if create(&info, null(), &mut instance) != 0 {
            return Ok(false);
        }
        let mut n = 0u32;
        let mut real = false;
        if enumerate(instance, &mut n, std::ptr::null_mut()) == 0 && n > 0 {
            let mut devices = vec![std::ptr::null_mut(); n as usize];
            if enumerate(instance, &mut n, devices.as_mut_ptr()) >= 0 {
                for d in devices.into_iter().take(n as usize) {
                    // VkPhysicalDeviceProperties is 824 bytes; deviceType is the u32 at offset 16.
                    let mut buf = [0u64; PROPS_U64];
                    props(d, buf.as_mut_ptr());
                    real |= (buf[2] & 0xffff_ffff) as u32 != TYPE_CPU;
                }
            }
        }
        destroy(instance, null());
        Ok(real)
    }
}
