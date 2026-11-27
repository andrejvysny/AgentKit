//! The contract between the launcher and `agentkit-sandbox-vmhelper`
//! (ADR 0040, section 3): one helper process per running machine.
//!
//! - argv: none. The helper refuses any argument.
//! - stdin: the first line is one [`HelperConfig`] as JSON. The launcher
//!   keeps the write end open for the machine's whole life; EOF means the
//!   launcher is gone or wants the machine stopped.
//! - stdout: [`HelperEvent`] JSON lines: `started`, then `connected`, and
//!   `stopped` last. Diagnostics go to stderr.
//! - Exit status: 0 only when the VM reached the stopped state; 1 when it
//!   ended in another state; 2 when the configuration or the start was
//!   refused; 3 when the stop took longer than its watchdog.
//!
//! The channel token travels only inside [`HelperConfig::cmdline`], on the
//! pipe: never in argv or the environment, never in an event.

use serde::{Deserialize, Serialize};

/// Version of [`HelperConfig`]; the helper refuses any other.
pub const HELPER_PROTOCOL: u32 = 1;
/// Guest vsock port of the supervisor's control listener. Only the helper
/// connects to it, once per VM lifetime.
pub const CONTROL_PORT: u32 = 1024;
/// Host vsock port the helper listens on for the guest's egress shim.
pub const EGRESS_PORT: u32 = 1025;
/// Longest stdin configuration line, in bytes.
pub const CONFIG_LINE_MAX_BYTES: usize = 64 * 1024;
/// Longest stdout event line the launcher reads, in bytes.
pub const EVENT_LINE_MAX_BYTES: usize = 4 * 1024;

/// What one helper runs, written as the first line on its stdin. Every
/// path is absolute.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperConfig {
    /// [`HELPER_PROTOCOL`].
    pub protocol: u32,
    /// The arm64 Linux kernel `Image`.
    pub kernel: String,
    /// The root disk, attached read-only as `vda`.
    pub root_disk: String,
    /// The workspace disk, read-write `vdb`.
    pub workspace_disk: String,
    /// The home disk, read-write `vdc`.
    pub home_disk: String,
    /// Virtual CPUs.
    pub cpus: u32,
    /// Guest memory in bytes.
    pub memory_bytes: u64,
    /// The kernel command line. Carries the channel token.
    pub cmdline: String,
    /// The adapter's control socket the bridge to guest port
    /// [`CONTROL_PORT`] connects to.
    pub control_socket: String,
    /// The adapter's egress socket every guest connection to
    /// [`EGRESS_PORT`] is spliced to.
    pub egress_socket: String,
    /// File the guest's `hvc0` console is written to.
    pub console_log: String,
}

impl std::fmt::Debug for HelperConfig {
    // The command line carries the channel token; it is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperConfig")
            .field("protocol", &self.protocol)
            .field("kernel", &self.kernel)
            .field("root_disk", &self.root_disk)
            .field("workspace_disk", &self.workspace_disk)
            .field("home_disk", &self.home_disk)
            .field("cpus", &self.cpus)
            .field("memory_bytes", &self.memory_bytes)
            .field("control_socket", &self.control_socket)
            .field("egress_socket", &self.egress_socket)
            .field("console_log", &self.console_log)
            .finish_non_exhaustive()
    }
}

/// One line of the helper's stdout. Tagged by `"event"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HelperEvent {
    /// The VM started.
    Started,
    /// The helper holds its one connection to guest port [`CONTROL_PORT`]
    /// and now connects it to the control socket. The adapter binds that
    /// socket only after `ensured`, so the launcher answers on this event
    /// and the helper retries its connect until a deadline.
    Connected,
    /// The VM is gone; the last event.
    Stopped {
        /// Why, for logs.
        reason: String,
    },
}

/// The kernel command line the launcher composes for one generation
/// (ADR 0040, section 4). The keys are fixed; `image_digest` is the same
/// string `ensure` must name and the guest answers in `hello`.
pub fn kernel_cmdline(generation: u64, image_digest: &str, token: &str) -> String {
    format!(
        "console=hvc0 root=/dev/vda rootfstype=ext4 ro init=/usr/local/sbin/agentkit-vm-init \
         panic=-1 quiet agentkit.generation={generation} agentkit.image={image_digest} \
         agentkit.token={token} agentkit.control_port={CONTROL_PORT} \
         agentkit.egress_port={EGRESS_PORT}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_has_the_fixed_keys() {
        let line = kernel_cmdline(7, "sha256:abc", "0123");
        assert_eq!(
            line,
            "console=hvc0 root=/dev/vda rootfstype=ext4 ro init=/usr/local/sbin/agentkit-vm-init \
             panic=-1 quiet agentkit.generation=7 agentkit.image=sha256:abc agentkit.token=0123 \
             agentkit.control_port=1024 agentkit.egress_port=1025"
        );
    }

    #[test]
    fn events_use_the_documented_lines() {
        let stopped = HelperEvent::Stopped {
            reason: "stdin closed".into(),
        };
        let lines = [
            (HelperEvent::Started, r#"{"event":"started"}"#),
            (HelperEvent::Connected, r#"{"event":"connected"}"#),
            (stopped, r#"{"event":"stopped","reason":"stdin closed"}"#),
        ];
        for (event, line) in lines {
            assert_eq!(serde_json::to_string(&event).unwrap(), line);
            assert_eq!(serde_json::from_str::<HelperEvent>(line).unwrap(), event);
        }
    }

    #[test]
    fn unknown_config_fields_are_refused_and_debug_hides_the_cmdline() {
        let config = HelperConfig {
            protocol: HELPER_PROTOCOL,
            kernel: "/k".into(),
            root_disk: "/r".into(),
            workspace_disk: "/w".into(),
            home_disk: "/h".into(),
            cpus: 2,
            memory_bytes: 1 << 31,
            cmdline: "agentkit.token=secret".into(),
            control_socket: "/c".into(),
            egress_socket: "/e".into(),
            console_log: "/l".into(),
        };
        assert!(!format!("{config:?}").contains("secret"));
        let mut value = serde_json::to_value(&config).unwrap();
        value["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<HelperConfig>(value).is_err());
    }
}
