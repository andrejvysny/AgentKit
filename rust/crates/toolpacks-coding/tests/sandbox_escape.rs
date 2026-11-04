//! Real `sandbox-exec` checks of the Seatbelt profile (macOS only): the
//! LaunchServices / AppleEvents / launchd / DNS escape routes stay closed
//! while ordinary build tooling keeps working. Skipped where `sandbox-exec`
//! is missing.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agentkit_toolpacks_coding::sandbox::{
    SANDBOX_EXEC, SandboxPolicy, sandbox_profile, toolchain_paths,
};

struct Fixture {
    root: PathBuf,
    scratch: PathBuf,
    profile: String,
}

impl Fixture {
    fn new(tag: &str, network: bool) -> Option<Self> {
        if !Path::new(SANDBOX_EXEC).exists() {
            eprintln!("skipped: {SANDBOX_EXEC} missing");
            return None;
        }
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("ak-sbx-{tag}-{}", std::process::id()));
        let root = base.join("root");
        let scratch = base.join("scratch");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let env: HashMap<String, String> = std::env::vars().collect();
        let profile = sandbox_profile(&SandboxPolicy {
            root: root.to_string_lossy().into_owned(),
            scratch: scratch.to_string_lossy().into_owned(),
            network,
            allow_git_writes: false,
            readable: toolchain_paths(&env),
        })
        .unwrap();
        Some(Self {
            root,
            scratch,
            profile,
        })
    }

    /// Run `bash -c script` inside the sandbox; `None` when it did not
    /// finish within the deadline (and was killed).
    fn run(&self, script: &str, home: Option<&Path>) -> Option<(i32, String)> {
        let mut command = Command::new(SANDBOX_EXEC);
        command
            .args(["-p", &self.profile, "/bin/bash", "-c", script])
            .current_dir(&self.root)
            .env("TMPDIR", &self.scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(home) = home {
            command.env("HOME", home);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if child.try_wait().unwrap().is_some() {
                let out = child.wait_with_output().unwrap();
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                return Some((out.status.code().unwrap_or(-1), text));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn succeeds(&self, what: &str, script: &str) {
        let result = self.run(script, Some(&self.scratch));
        assert!(
            matches!(result, Some((0, _))),
            "{what} must succeed in the sandbox: {result:?}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
    }
}

#[test]
fn launch_services_apple_events_and_launchd_are_unreachable() {
    let Some(fx) = Fixture::new("escape", false) else {
        return;
    };
    // LaunchServices lookup of an installed app must not resolve.
    let open = fx.run("open -a TextEdit", None);
    assert!(
        !matches!(open, Some((0, _))),
        "`open` must fail in the sandbox: {open:?}"
    );
    // AppleEvents to Terminal: if the escape worked, the (unsandboxed)
    // Terminal would create the marker. A hang or error both mean blocked.
    let marker = fx.root.join("terminal-ran");
    let script = format!(
        "osascript -e 'tell app \"Terminal\" to do script \"touch {}\"'",
        marker.display()
    );
    let osa = fx.run(&script, None);
    assert!(
        !matches!(osa, Some((0, _))),
        "osascript must not drive Terminal: {osa:?}"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !marker.exists(),
        "Terminal executed a command for the sandbox"
    );
    // launchd job submission.
    let submit = fx.run(
        "launchctl submit -l ak.sandbox.escape -- /usr/bin/true",
        None,
    );
    assert!(
        !matches!(submit, Some((0, _))),
        "launchctl submit must fail: {submit:?}"
    );
    let listed = Command::new("/bin/launchctl").arg("list").output().unwrap();
    assert!(
        !String::from_utf8_lossy(&listed.stdout).contains("ak.sandbox.escape"),
        "a launchd job was registered from inside the sandbox"
    );
    let _ = Command::new("/bin/launchctl")
        .args(["remove", "ak.sandbox.escape"])
        .output();
}

#[test]
fn dns_lookups_fail_when_the_network_is_off() {
    let Some(fx) = Fixture::new("dns", false) else {
        return;
    };
    let result = fx.run(
        "dscacheutil -q host -a name example.com | grep -q ip_address",
        None,
    );
    assert!(
        !matches!(result, Some((0, _))),
        "name resolution must not work offline: {result:?}"
    );
}

#[test]
fn ordinary_tooling_still_works() {
    let Some(fx) = Fixture::new("legit", false) else {
        return;
    };
    fx.succeeds("git --version", "git --version");
    fx.succeeds("workspace write", "echo hi > f && test \"$(cat f)\" = hi");
    fx.succeeds("python3", "python3 -c 1");
    fx.succeeds(
        "node (if installed)",
        "! command -v node >/dev/null || node -e 1",
    );
    fx.succeeds(
        "cc",
        "printf 'int main(){return 0;}' > a.c && cc a.c -o a.out && ./a.out",
    );
    fx.succeeds("pipes", "echo a | sed s/a/b/ | cat >/dev/null");
    if let Some(cargo) = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cargo/bin/cargo"))
        .filter(|path| path.exists())
    {
        let result = fx.run(
            &format!("{} --version", cargo.display()),
            std::env::var_os("HOME").as_deref().map(Path::new),
        );
        assert!(
            matches!(result, Some((0, _))),
            "cargo --version: {result:?}"
        );
    }
    // `git status` in a repo created outside the sandbox (`.git` is
    // read-only inside it by design).
    let init = Command::new("git")
        .args(["init", "-q"])
        .arg(&fx.root)
        .status()
        .unwrap();
    assert!(init.success());
    fx.succeeds("git status", "git status --short");
}

#[test]
fn ordinary_tooling_still_works_with_the_network_allowed() {
    let Some(fx) = Fixture::new("legit-net", true) else {
        return;
    };
    fx.succeeds("git --version", "git --version");
    fx.succeeds("curl --version", "curl --version >/dev/null");
}
