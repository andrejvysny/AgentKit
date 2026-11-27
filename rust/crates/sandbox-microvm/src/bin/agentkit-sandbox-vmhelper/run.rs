//! The helper's life: read the configuration, boot the VM, bridge the
//! control channel (host initiated, guest port 1024, once) and egress
//! (guest initiated, host port 1025, any number), wait for what ends it,
//! then stop: close the control bridge (the guest's supervisor exits and
//! its init powers off), ask the guest to stop, after 3 s force the stop,
//! exit.

use std::io::{BufRead, BufReader, Read, Stdin};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use agentkit_sandbox_microvm::helper::{
    CONFIG_LINE_MAX_BYTES, CONTROL_PORT, EGRESS_PORT, HelperConfig, HelperEvent,
};
use objc2_virtualization::{VZVirtualMachine, VZVirtualMachineState};

use crate::bridge::{self, Closer};
use crate::delegate::{Channels, HelperDelegate};
use crate::events::{diag, emit};
use crate::lifetime::{self, Event};
use crate::vm::{Vm, VsockConn};

/// How long the guest gets to listen on the control port after start.
const GUEST_LISTEN_DEADLINE: Duration = Duration::from_secs(30);
/// How long the adapter gets to bind the control socket after
/// `connected`.
const CONTROL_SOCKET_DEADLINE: Duration = Duration::from_secs(30);
/// Pause between vsock connect attempts while the guest boots.
const VSOCK_RETRY: Duration = Duration::from_millis(10);
/// Longest wait for one attempt's answer before the next attempt.
const VSOCK_ATTEMPT_WAIT: Duration = Duration::from_secs(2);
/// The guest's time to power off after the control channel closed.
const GUEST_STOP: Duration = Duration::from_secs(3);
/// Time a forced stop may take.
const FORCE_STOP: Duration = Duration::from_secs(5);
/// Bound of the whole stop.
const STOP_WATCHDOG: Duration = Duration::from_secs(15);
/// Most egress connections bridged at once (as the container's shim).
const EGRESS_MAX: usize = 64;

/// Run the helper; answers the exit status.
pub(crate) fn main() -> i32 {
    if let Err(error) = lifetime::block_sigterm() {
        return refuse(&format!("cannot block SIGTERM: {error}"));
    }
    if std::env::args_os().len() > 1 {
        return refuse("the helper takes no arguments; its configuration comes on stdin");
    }
    let mut stdin = BufReader::new(std::io::stdin());
    let config = match read_config(&mut stdin) {
        Ok(config) => config,
        Err(why) => return refuse(&why),
    };
    let (events_tx, events) = mpsc::channel();
    lifetime::watch_sigterm(events_tx.clone());
    lifetime::watch_stdin(stdin, events_tx.clone());
    lifetime::watch_parent(events_tx.clone());
    let (egress_tx, egress) = mpsc::channel();
    let delegate = HelperDelegate::new(Channels {
        events: events_tx.clone(),
        egress: egress_tx,
    });
    let vm = match boot(&config, delegate) {
        Ok(vm) => Arc::new(vm),
        Err(why) => return refuse(&why),
    };
    emit(&HelperEvent::Started);
    let closer = Closer::default();
    match vm.listen(EGRESS_PORT) {
        Ok(()) => start_egress(egress, PathBuf::from(&config.egress_socket)),
        Err(why) => {
            let _ = events_tx.send(Event::Stop(format!("egress listener: {why}")));
        }
    }
    start_control(Arc::clone(&vm), &config, closer.clone(), events_tx);
    let reason = wait_for_end(&events);
    stop(&vm, &closer, reason)
}

/// The first stdin line as a checked configuration. An error never
/// repeats the line: it carries the channel token.
fn read_config(stdin: &mut BufReader<Stdin>) -> Result<HelperConfig, String> {
    let mut line = Vec::new();
    let limit = (CONFIG_LINE_MAX_BYTES + 1) as u64;
    stdin
        .by_ref()
        .take(limit)
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("reading the configuration failed: {error}"))?;
    if line.last() != Some(&b'\n') {
        return Err("stdin ended or overflowed before one configuration line".into());
    }
    let config: HelperConfig = serde_json::from_slice(&line).map_err(|error| {
        format!(
            "the configuration line is not a valid configuration ({:?} at column {})",
            error.classify(),
            error.column()
        )
    })?;
    crate::config::check(&config)?;
    Ok(config)
}

fn boot(
    config: &HelperConfig,
    delegate: objc2::rc::Retained<HelperDelegate>,
) -> Result<Vm, String> {
    // SAFETY: a class method without preconditions.
    if !unsafe { VZVirtualMachine::isSupported() } {
        return Err("Virtualization.framework is not supported on this host".into());
    }
    let configuration = crate::config::build(config)?;
    let vm = Vm::new(&configuration, delegate);
    vm.start()
        .map_err(|why| format!("the VM did not start: {why}"))?;
    Ok(vm)
}

/// Bridge every guest connection to the egress port to a new connection
/// to the egress socket.
fn start_egress(connections: Receiver<VsockConn>, socket: PathBuf) {
    thread::spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        for conn in connections {
            if active.load(Ordering::SeqCst) >= EGRESS_MAX {
                continue;
            }
            active.fetch_add(1, Ordering::SeqCst);
            let (active, socket) = (Arc::clone(&active), socket.clone());
            thread::spawn(move || {
                if let Err(error) = bridge_egress(&conn, &socket) {
                    diag(&format!("egress connection failed: {error}"));
                }
                drop(conn);
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
}

fn bridge_egress(conn: &VsockConn, socket: &Path) -> std::io::Result<()> {
    let host = UnixStream::connect(socket)?;
    bridge::splice_half_close(conn.stream.try_clone()?, host)
}

/// The one control bridge; when it ends, the VM stops.
fn start_control(vm: Arc<Vm>, config: &HelperConfig, closer: Closer, events: Sender<Event>) {
    let socket = PathBuf::from(&config.control_socket);
    thread::spawn(move || {
        let reason = match control(&vm, &socket, &closer) {
            Ok(()) => "the control channel closed".to_owned(),
            Err(why) => why,
        };
        let _ = events.send(Event::Stop(reason));
    });
}

fn control(vm: &Vm, socket: &Path, closer: &Closer) -> Result<(), String> {
    let guest = connect_guest(vm, closer)?;
    if !closer.add(&guest.stream) {
        return Err("stopping".into());
    }
    emit(&HelperEvent::Connected);
    let deadline = Instant::now() + CONTROL_SOCKET_DEADLINE;
    let host = bridge::connect_until(socket, deadline, || closer.is_closed())
        .map_err(|error| format!("the control socket did not accept: {error}"))?;
    if !closer.add(&host) {
        return Err("stopping".into());
    }
    let guest_stream = guest
        .stream
        .try_clone()
        .map_err(|error| error.to_string())?;
    bridge::splice_until_either_closes(guest_stream, host).map_err(|error| error.to_string())?;
    // The framework's connection object lives until the bridge ended.
    drop(guest);
    Ok(())
}

/// Connect to the guest's control port, retrying while the guest boots.
/// Every attempt answers on one channel, so an answer that comes after
/// its attempt's wait is still taken; the guest accepts one connection.
fn connect_guest(vm: &Vm, closer: &Closer) -> Result<VsockConn, String> {
    let deadline = Instant::now() + GUEST_LISTEN_DEADLINE;
    let (answer, answers) = mpsc::channel();
    let mut last = String::from("no answer");
    while Instant::now() < deadline {
        if closer.is_closed() {
            return Err("stopping".into());
        }
        vm.connect(CONTROL_PORT, answer.clone());
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(VSOCK_ATTEMPT_WAIT);
        match answers.recv_timeout(wait) {
            Ok(Ok(conn)) => return Ok(conn),
            Ok(Err(why)) => {
                last = why;
                thread::sleep(VSOCK_RETRY);
            }
            Err(_) => last = "the framework did not answer".into(),
        }
    }
    Err(format!(
        "guest port {CONTROL_PORT} did not accept within {} s: {last}",
        GUEST_LISTEN_DEADLINE.as_secs()
    ))
}

fn wait_for_end(events: &Receiver<Event>) -> String {
    match events.recv() {
        Ok(Event::Stop(reason)) => reason,
        Ok(Event::GuestStopped) => "the guest powered off".into(),
        Ok(Event::VmFailed(why)) => format!("the VM stopped on an error: {why}"),
        Err(_) => "every watcher ended".into(),
    }
}

/// Stop the VM and answer the exit status: 0 only when it reached the
/// stopped state.
fn stop(vm: &Vm, closer: &Closer, reason: String) -> i32 {
    lifetime::arm_watchdog(STOP_WATCHDOG);
    closer.close();
    if !ended(vm.state()) {
        if !vm.request_stop() {
            diag("the guest could not be asked to stop");
        }
        wait_ended(vm, GUEST_STOP);
    }
    if !ended(vm.state()) {
        vm.force_stop(FORCE_STOP);
    }
    let state = vm.state();
    emit(&HelperEvent::Stopped { reason });
    if state == VZVirtualMachineState::Stopped {
        0
    } else {
        diag(&format!("the VM ended in state {}", state.0));
        1
    }
}

fn ended(state: VZVirtualMachineState) -> bool {
    state == VZVirtualMachineState::Stopped || state == VZVirtualMachineState::Error
}

fn wait_ended(vm: &Vm, within: Duration) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline && !ended(vm.state()) {
        thread::sleep(Duration::from_millis(50));
    }
}

/// Report a refused start and answer status 2.
fn refuse(why: &str) -> i32 {
    diag(why);
    emit(&HelperEvent::Stopped {
        reason: why.to_owned(),
    });
    2
}
