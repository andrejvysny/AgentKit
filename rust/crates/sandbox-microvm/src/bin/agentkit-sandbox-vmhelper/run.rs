//! The helper's life: read the configuration, boot the VM, bridge the
//! control channel (host initiated, guest port 1024, once) and egress
//! (guest initiated, host port 1025, any number), wait for what ends it,
//! then stop: close the control bridge (the guest's supervisor exits and
//! its init powers off), ask the guest to stop, after 3 s force the stop,
//! let the console drain settle, exit.

use std::io::{BufRead, BufReader, Read, Stdin};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use agentkit_sandbox_microvm::helper::{
    CONFIG_LINE_MAX_BYTES, CONTROL_PORT, EGRESS_PORT, HelperConfig, HelperEvent,
};
use objc2_virtualization::{VZVirtualMachine, VZVirtualMachineState};

use crate::bridge::{self, Closer};
use crate::console::Drain;
use crate::delegate::{Channels, HelperDelegate};
use crate::egress::{Bridge, DiagLimit, EgressSlots, IdleWatch, Slot};
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
/// How often a pending connect attempt checks whether the helper stops.
const VSOCK_ANSWER_POLL: Duration = Duration::from_millis(250);
/// The guest's time to power off after the control channel closed.
const GUEST_STOP: Duration = Duration::from_secs(3);
/// Time a forced stop may take.
const FORCE_STOP: Duration = Duration::from_secs(5);
/// Bound of the whole stop.
const STOP_WATCHDOG: Duration = Duration::from_secs(15);
/// Most egress connections bridged at once (as the container's shim).
const EGRESS_MAX: usize = 64;
/// An egress bridge no byte crossed for this long is shut down.
const EGRESS_IDLE: Duration = Duration::from_secs(10 * 60);
/// How often idle egress bridges are looked for.
const EGRESS_SWEEP: Duration = Duration::from_secs(10);
/// At most one egress diagnostic line per this long.
const EGRESS_DIAG_EVERY: Duration = Duration::from_secs(10);
/// Open descriptors the helper asks for: a bridge holds two, the
/// framework one more per connection, and macOS starts a process at 256.
const OPEN_FILES: u64 = 4096;
/// At a stop, the console counts as drained after this long without
/// output, or after the second bound.
const CONSOLE_QUIET: Duration = Duration::from_millis(100);
const CONSOLE_SETTLE: Duration = Duration::from_millis(500);

/// Run the helper; answers the exit status.
pub(crate) fn main() -> i32 {
    if let Err(error) = lifetime::block_sigterm() {
        return refuse(&format!("cannot block SIGTERM: {error}"));
    }
    if std::env::args_os().len() > 1 {
        return refuse("the helper takes no arguments; its configuration comes on stdin");
    }
    raise_open_files();
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
    let egress_diag = Arc::new(DiagLimit::new(EGRESS_DIAG_EVERY));
    let delegate = HelperDelegate::new(Channels {
        events: events_tx.clone(),
        egress: egress_tx,
        slots: EgressSlots::new(EGRESS_MAX),
        egress_diag: Arc::clone(&egress_diag),
    });
    let (vm, console) = match boot(&config, delegate) {
        Ok((vm, console)) => (Arc::new(vm), console),
        Err(why) => return refuse(&why),
    };
    emit(&HelperEvent::Started);
    let closer = Closer::default();
    match vm.listen(EGRESS_PORT) {
        Ok(()) => start_egress(egress, PathBuf::from(&config.egress_socket), egress_diag),
        Err(why) => {
            let _ = events_tx.send(Event::Stop(format!("egress listener: {why}")));
        }
    }
    start_control(Arc::clone(&vm), &config, closer.clone(), events_tx);
    let reason = wait_for_end(&events);
    stop(&vm, &closer, &console, reason)
}

/// Room for every bridge at once.
fn raise_open_files() {
    match agentkit_sys::raise_open_file_limit(OPEN_FILES) {
        Ok(limit) if limit < OPEN_FILES => diag(&format!("open files are limited to {limit}")),
        Ok(_) => {}
        Err(error) => diag(&format!("the open file limit was not raised: {error}")),
    }
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

/// The started VM and the drain of its console.
fn boot(
    config: &HelperConfig,
    delegate: objc2::rc::Retained<HelperDelegate>,
) -> Result<(Vm, Arc<Drain>), String> {
    // SAFETY: a class method without preconditions.
    if !unsafe { VZVirtualMachine::isSupported() } {
        return Err("Virtualization.framework is not supported on this host".into());
    }
    let (configuration, console) = crate::config::build(config)?;
    let vm = Vm::new(&configuration, delegate);
    vm.start()
        .map_err(|why| format!("the VM did not start: {why}"))?;
    Ok((vm, console))
}

/// Bridge every guest connection to the egress port (each came with its
/// slot) to a new connection to the egress socket, and shut down bridges
/// left idle.
fn start_egress(
    connections: Receiver<(VsockConn, Slot)>,
    socket: PathBuf,
    diag_limit: Arc<DiagLimit>,
) {
    thread::spawn(move || {
        let idle = Arc::new(IdleWatch::new(EGRESS_IDLE));
        loop {
            match connections.recv_timeout(EGRESS_SWEEP) {
                Ok((conn, slot)) => {
                    let (socket, idle) = (socket.clone(), Arc::clone(&idle));
                    let diag_limit = Arc::clone(&diag_limit);
                    thread::spawn(move || {
                        if let Err(error) = bridge_egress(conn, &socket, &idle) {
                            limited(&diag_limit, &format!("egress connection failed: {error}"));
                        }
                        drop(slot);
                    });
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            idle.sweep(Instant::now());
        }
    });
}

fn bridge_egress(conn: VsockConn, socket: &Path, idle: &IdleWatch) -> std::io::Result<()> {
    let host = UnixStream::connect(socket)?;
    // Moves the stream out; the rest of `conn`, the framework's connection
    // object, is dropped when this function returns, after the bridge.
    let bridge = Bridge::new(conn.stream, host);
    idle.watch(&bridge);
    bridge.run();
    Ok(())
}

/// One diagnostic line through `limit`.
fn limited(limit: &DiagLimit, text: &str) {
    if let Some(line) = limit.line(Instant::now(), text) {
        diag(&line);
    }
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
    // Moves the stream out; the rest of `guest`, the framework's connection
    // object, is dropped when this function returns, after the bridge.
    let guest_stream = Arc::new(guest.stream);
    if !closer.add(&guest_stream) {
        return Err("stopping".into());
    }
    emit(&HelperEvent::Connected);
    let deadline = Instant::now() + CONTROL_SOCKET_DEADLINE;
    let host = bridge::connect_until(socket, deadline, || closer.is_closed())
        .map_err(|error| format!("the control socket did not accept: {error}"))?;
    let host = Arc::new(host);
    if !closer.add(&host) {
        return Err("stopping".into());
    }
    bridge::splice_until_either_closes(guest_stream, host);
    Ok(())
}

/// Connect to the guest's control port, retrying while the guest boots.
/// One attempt at a time: the next is asked for only after the framework
/// answered the previous one, so no two attempts race for the guest's one
/// accepted connection.
fn connect_guest(vm: &Vm, closer: &Closer) -> Result<VsockConn, String> {
    let deadline = Instant::now() + GUEST_LISTEN_DEADLINE;
    let (answer, answers) = mpsc::channel();
    let (mut pending, mut last) = (false, String::from("no answer"));
    while Instant::now() < deadline {
        if closer.is_closed() {
            return Err("stopping".into());
        }
        if !pending {
            vm.connect(CONTROL_PORT, answer.clone());
            pending = true;
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(VSOCK_ANSWER_POLL);
        match answers.recv_timeout(wait) {
            Ok(Ok(conn)) => return Ok(conn),
            Ok(Err(why)) => {
                (pending, last) = (false, why);
                thread::sleep(VSOCK_RETRY);
            }
            Err(_) => last = "the framework has not answered the attempt".into(),
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
fn stop(vm: &Vm, closer: &Closer, console: &Drain, reason: String) -> i32 {
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
    console.settle(CONSOLE_QUIET, CONSOLE_SETTLE);
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
