//! Building the Virtualization.framework configuration (ADR 0040, section
//! 3): Linux boot loader with the launcher's command line and no initrd;
//! `vda` root disk read-only, `vdb` workspace and `vdc` home read-write;
//! `hvc0` console to a file; one virtio socket device, one entropy device,
//! one traditional balloon; generic platform; no network device.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use agentkit_sandbox_microvm::helper::{HELPER_PROTOCOL, HelperConfig};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use objc2_virtualization::{
    VZDiskImageCachingMode, VZDiskImageStorageDeviceAttachment, VZDiskImageSynchronizationMode,
    VZEntropyDeviceConfiguration, VZFileSerialPortAttachment, VZGenericPlatformConfiguration,
    VZLinuxBootLoader, VZMemoryBalloonDeviceConfiguration, VZSerialPortConfiguration,
    VZSocketDeviceConfiguration, VZStorageDeviceConfiguration, VZVirtioBlockDeviceConfiguration,
    VZVirtioConsoleDeviceSerialPortConfiguration, VZVirtioEntropyDeviceConfiguration,
    VZVirtioSocketDeviceConfiguration, VZVirtioTraditionalMemoryBalloonDeviceConfiguration,
    VZVirtualMachineConfiguration,
};

/// Refuse a configuration the launcher would never write.
pub(crate) fn check(config: &HelperConfig) -> Result<(), String> {
    if config.protocol != HELPER_PROTOCOL {
        return Err(format!("unknown protocol {}", config.protocol));
    }
    let paths = [
        &config.kernel,
        &config.root_disk,
        &config.workspace_disk,
        &config.home_disk,
        &config.control_socket,
        &config.egress_socket,
        &config.console_log,
    ];
    if !paths.iter().all(|path| Path::new(path).is_absolute()) {
        return Err("every path must be absolute".into());
    }
    if config.cpus == 0 || config.memory_bytes == 0 {
        return Err("cpus and memory_bytes must be positive".into());
    }
    Ok(())
}

/// The validated VM configuration.
pub(crate) fn build(
    config: &HelperConfig,
) -> Result<Retained<VZVirtualMachineConfiguration>, String> {
    let disks = [
        disk(&config.root_disk, true)?,
        disk(&config.workspace_disk, false)?,
        disk(&config.home_disk, false)?,
    ];
    let console = console(&config.console_log)?;
    // SAFETY: every object is created and wired on this thread before a VM
    // exists, and every argument is a valid retained object of the type
    // the setter takes.
    unsafe {
        let vm = VZVirtualMachineConfiguration::new();
        vm.setBootLoader(Some(&boot_loader(config)));
        vm.setCPUCount(config.cpus as usize);
        vm.setMemorySize(config.memory_bytes);
        vm.setPlatform(&VZGenericPlatformConfiguration::new());
        let disks: Vec<&VZStorageDeviceConfiguration> = disks.iter().map(|d| &***d).collect();
        vm.setStorageDevices(&NSArray::from_slice(&disks));
        let serials: [&VZSerialPortConfiguration; 1] = [&console];
        vm.setSerialPorts(&NSArray::from_slice(&serials));
        let vsock = VZVirtioSocketDeviceConfiguration::new();
        let sockets: [&VZSocketDeviceConfiguration; 1] = [&vsock];
        vm.setSocketDevices(&NSArray::from_slice(&sockets));
        let entropy = VZVirtioEntropyDeviceConfiguration::new();
        let entropies: [&VZEntropyDeviceConfiguration; 1] = [&entropy];
        vm.setEntropyDevices(&NSArray::from_slice(&entropies));
        let balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();
        let balloons: [&VZMemoryBalloonDeviceConfiguration; 1] = [&balloon];
        vm.setMemoryBalloonDevices(&NSArray::from_slice(&balloons));
        vm.validateWithError().map_err(|e| ns_error(&e))?;
        Ok(vm)
    }
}

fn boot_loader(config: &HelperConfig) -> Retained<VZLinuxBootLoader> {
    // SAFETY: plain initialiser and setter on valid retained arguments; no
    // initial ramdisk, the kernel has its drivers built in.
    unsafe {
        let boot = VZLinuxBootLoader::initWithKernelURL(
            VZLinuxBootLoader::alloc(),
            &file_url(&config.kernel),
        );
        boot.setCommandLine(&NSString::from_str(&config.cmdline));
        boot
    }
}

/// A virtio block device on `path`. The root is shared read-only between
/// VMs (cached, no sync); a read-write disk is flushed to the host on
/// every guest flush, so an acknowledged write survives a crash.
fn disk(path: &str, read_only: bool) -> Result<Retained<VZVirtioBlockDeviceConfiguration>, String> {
    let (caching, sync) = if read_only {
        (
            VZDiskImageCachingMode::Cached,
            VZDiskImageSynchronizationMode::None,
        )
    } else {
        (
            VZDiskImageCachingMode::Automatic,
            VZDiskImageSynchronizationMode::Full,
        )
    };
    // SAFETY: plain Objective-C initialisers on valid retained arguments.
    unsafe {
        let attachment =
            VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &file_url(path),
                read_only,
                caching,
                sync,
            )
            .map_err(|e| format!("disk {path}: {}", ns_error(&e)))?;
        Ok(VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &attachment,
        ))
    }
}

/// The `hvc0` console, written to `path` (created 0600, truncated).
fn console(path: &str) -> Result<Retained<VZVirtioConsoleDeviceSerialPortConfiguration>, String> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("console log {path}: {error}"))?;
    // SAFETY: plain Objective-C initialisers and a setter on valid
    // retained arguments.
    unsafe {
        let attachment = VZFileSerialPortAttachment::initWithURL_append_error(
            VZFileSerialPortAttachment::alloc(),
            &file_url(path),
            false,
        )
        .map_err(|e| format!("console log {path}: {}", ns_error(&e)))?;
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        serial.setAttachment(Some(&attachment));
        Ok(serial)
    }
}

fn file_url(path: &str) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(path))
}

/// The error's description, for a reason line.
pub(crate) fn ns_error(error: &NSError) -> String {
    error.localizedDescription().to_string()
}
