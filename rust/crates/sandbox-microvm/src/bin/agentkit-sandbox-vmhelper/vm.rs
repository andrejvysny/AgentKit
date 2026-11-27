//! The VM and its serial dispatch queue. Virtualization.framework requires
//! every call on a `VZVirtualMachine` (and its devices) to run on the
//! queue it was created with, and runs every completion and delegate
//! callback there; [`Vm`] only ever touches the VM inside blocks it
//! submits to that queue.

use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, Message};
use objc2_foundation::NSError;
use objc2_virtualization::{
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtioSocketListenerDelegate, VZVirtualMachine, VZVirtualMachineConfiguration,
    VZVirtualMachineDelegate, VZVirtualMachineState,
};

use crate::config::ns_error;
use crate::delegate::HelperDelegate;

/// Longest wait for the framework's answer to a start.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// A vsock connection: the framework's object (kept alive, since it owns
/// the original descriptor) and a duplicate of its descriptor as a stream.
pub(crate) struct VsockConn {
    _connection: Retained<VZVirtioSocketConnection>,
    pub(crate) stream: UnixStream,
}

// SAFETY: the connection object is never messaged after it was retained
// on the VM queue; it is only kept alive and released, and Objective-C
// reference counting is thread-safe. The stream is a plain descriptor.
unsafe impl Send for VsockConn {}

impl VsockConn {
    /// Retain `connection` and duplicate its descriptor. Call on the VM
    /// queue (in a completion or delegate callback).
    pub(crate) fn adopt(connection: &VZVirtioSocketConnection) -> Option<Self> {
        // SAFETY: `fileDescriptor` is read on the VM queue, where the
        // callback runs; `dup` reads no memory and the result is checked.
        let fd = unsafe { libc::dup(connection.fileDescriptor()) };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` is a fresh descriptor this stream alone owns; a
        // vsock socket supports every call a `UnixStream` makes (read,
        // write, shutdown, dup).
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        Some(Self {
            _connection: connection.retain(),
            stream,
        })
    }
}

/// One VM, its queue and its delegate (the framework holds the delegate
/// weakly, so it lives here).
pub(crate) struct Vm {
    queue: DispatchRetained<DispatchQueue>,
    vm: Retained<VZVirtualMachine>,
    delegate: Retained<HelperDelegate>,
    listener: Retained<VZVirtioSocketListener>,
}

// SAFETY: `Vm` touches the VM, the delegate and the listener only inside
// blocks it runs on the VM's serial queue (`on_queue`); the queue handle
// is thread-safe, and the retained objects are only cloned and released
// elsewhere, which Objective-C reference counting allows on any thread.
unsafe impl Send for Vm {}
// SAFETY: as for `Send`; every method takes `&self` and serializes its VM
// access on the queue.
unsafe impl Sync for Vm {}

impl Vm {
    /// Create the VM on a new serial queue with `delegate`.
    pub(crate) fn new(
        config: &VZVirtualMachineConfiguration,
        delegate: Retained<HelperDelegate>,
    ) -> Self {
        let queue = DispatchQueue::new("agentkit.vmhelper", None);
        // SAFETY: `config` passed validation and `queue` is serial (no
        // attribute); from here on the VM is used only through `queue`.
        let vm = unsafe {
            VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), config, &queue)
        };
        // SAFETY: a plain initialiser; the listener is configured below.
        let listener = unsafe { VZVirtioSocketListener::new() };
        let this = Self {
            queue,
            vm,
            delegate,
            listener,
        };
        this.on_queue(|vm, delegate, _| {
            let delegate: &ProtocolObject<dyn VZVirtualMachineDelegate> =
                ProtocolObject::from_ref(delegate);
            // SAFETY: set on the VM queue; the delegate outlives the VM
            // (both live in this struct).
            unsafe { vm.setDelegate(Some(delegate)) };
        });
        this
    }

    /// Run `work` on the VM queue and wait for it.
    fn on_queue<T: Send>(
        &self,
        work: impl FnOnce(&VZVirtualMachine, &HelperDelegate, &VZVirtioSocketListener) -> T + Send,
    ) -> T {
        let mut out = None;
        let (this, slot) = (self, &mut out);
        self.queue.exec_sync(move || {
            // Rebind so the closure holds `&Vm` (which is `Send`), not
            // its non-`Send` fields.
            let (this, slot) = (this, slot);
            *slot = Some(work(&this.vm, &this.delegate, &this.listener));
        });
        out.expect("exec_sync runs its block before it returns")
    }

    /// Start the VM and wait for the completion.
    pub(crate) fn start(&self) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.on_queue(move |vm, _, _| {
            let block = completion(tx);
            // SAFETY: called on the VM queue with a valid completion block.
            unsafe { vm.startWithCompletionHandler(&block) };
        });
        rx.recv_timeout(START_TIMEOUT)
            .map_err(|_| "the start did not complete".to_owned())?
    }

    /// Listen on host vsock `port`: every guest connection goes to the
    /// delegate.
    pub(crate) fn listen(&self, port: u32) -> Result<(), String> {
        self.on_queue(move |vm, delegate, listener| {
            let delegate: &ProtocolObject<dyn VZVirtioSocketListenerDelegate> =
                ProtocolObject::from_ref(delegate);
            // SAFETY: devices are read and set on the VM queue; the
            // delegate and listener outlive the VM (both live in `Vm`).
            unsafe {
                listener.setDelegate(Some(delegate));
                let device = socket_device(vm)?;
                device.setSocketListener_forPort(listener, port);
            }
            Ok(())
        })
    }

    /// Ask for one connection to guest vsock `port`; the framework's
    /// answer goes to `answer` whenever it comes.
    pub(crate) fn connect(&self, port: u32, answer: Sender<Result<VsockConn, String>>) {
        self.on_queue(move |vm, _, _| {
            // SAFETY: this block runs on the VM queue.
            let device = match unsafe { socket_device(vm) } {
                Ok(device) => device,
                Err(why) => {
                    let _ = answer.send(Err(why));
                    return;
                }
            };
            let block = RcBlock::new(
                move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
                    // SAFETY: the framework passes either a valid connection
                    // or a valid error, each alive for the call.
                    let outcome = match unsafe { (conn.as_ref(), err.as_ref()) } {
                        (Some(conn), _) => {
                            VsockConn::adopt(conn).ok_or_else(|| "dup failed".to_owned())
                        }
                        (None, Some(err)) => Err(ns_error(err)),
                        (None, None) => Err("no connection and no error".to_owned()),
                    };
                    let _ = answer.send(outcome);
                },
            );
            // SAFETY: called on the VM queue with a valid completion block.
            unsafe { device.connectToPort_completionHandler(port, &block) };
        });
    }

    /// The VM's state.
    pub(crate) fn state(&self) -> VZVirtualMachineState {
        // SAFETY: read on the VM queue.
        self.on_queue(|vm, _, _| unsafe { vm.state() })
    }

    /// Ask the guest to power off; false when the VM cannot take the
    /// request in its state.
    pub(crate) fn request_stop(&self) -> bool {
        self.on_queue(|vm, _, _| {
            // SAFETY: called on the VM queue; `canRequestStop` guards the
            // states the request is invalid in.
            unsafe { vm.canRequestStop() && vm.requestStopWithError().is_ok() }
        })
    }

    /// Stop the VM at once (destructive) and wait up to `within`.
    pub(crate) fn force_stop(&self, within: Duration) {
        let (tx, rx) = mpsc::channel();
        self.on_queue(move |vm, _, _| {
            // SAFETY: called on the VM queue; `canStop` guards the states
            // a stop is invalid in.
            if !unsafe { vm.canStop() } {
                return drop(tx.send(Ok(())));
            }
            let block = completion(tx);
            // SAFETY: called on the VM queue with a valid completion block.
            unsafe { vm.stopWithCompletionHandler(&block) };
        });
        let _ = rx.recv_timeout(within);
    }
}

/// A completion block that sends its error (or success) on `tx`.
fn completion(tx: Sender<Result<(), String>>) -> RcBlock<dyn Fn(*mut NSError)> {
    RcBlock::new(move |err: *mut NSError| {
        // SAFETY: the framework passes null or a valid error alive for the
        // call.
        let outcome = match unsafe { err.as_ref() } {
            None => Ok(()),
            Some(err) => Err(ns_error(err)),
        };
        let _ = tx.send(outcome);
    })
}

/// The VM's one virtio socket device.
///
/// # Safety
///
/// Call on the VM queue.
unsafe fn socket_device(vm: &VZVirtualMachine) -> Result<Retained<VZVirtioSocketDevice>, String> {
    // SAFETY: the caller runs on the VM queue.
    let first = unsafe { vm.socketDevices() }.firstObject();
    first
        .ok_or_else(|| "the VM has no socket device".to_owned())?
        .downcast::<VZVirtioSocketDevice>()
        .map_err(|_| "the socket device is not a virtio socket device".to_owned())
}
