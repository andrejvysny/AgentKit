//! The one Objective-C class the helper defines: the VM's delegate (the
//! guest stopped, or the VM stopped on an error) and the egress listener's
//! delegate (a guest connection to the egress port). Both callbacks run on
//! the VM queue and only hand their news to Rust channels. An egress
//! connection beyond the bridge bound is refused right here.

use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSError, NSObjectProtocol};
use objc2_virtualization::{
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtioSocketListenerDelegate, VZVirtualMachine, VZVirtualMachineDelegate,
};

use crate::config::ns_error;
use crate::egress::{DiagLimit, EgressSlots, Slot};
use crate::events::diag;
use crate::lifetime::Event;
use crate::vm::VsockConn;

/// Where the callbacks send what they learn.
pub(crate) struct Channels {
    pub(crate) events: Sender<Event>,
    /// Accepted egress connections, each with its slot.
    pub(crate) egress: Sender<(VsockConn, Slot)>,
    pub(crate) slots: EgressSlots,
    /// Egress diagnostics, shared with the bridges.
    pub(crate) egress_diag: Arc<DiagLimit>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `HelperDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = Channels]
    pub(crate) struct HelperDelegate;

    // SAFETY: NSObject's own protocol, inherited unchanged.
    unsafe impl NSObjectProtocol for HelperDelegate {}

    // SAFETY: both methods are optional; the selectors and argument types
    // are the ones the protocol declares (`objc2-virtualization`).
    unsafe impl VZVirtualMachineDelegate for HelperDelegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            let _ = self.ivars().events.send(Event::GuestStopped);
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, error: &NSError) {
            let _ = self.ivars().events.send(Event::VmFailed(ns_error(error)));
        }
    }

    // SAFETY: the one optional method, with the selector and argument types
    // the protocol declares; it keeps an accepted connection retained, as
    // the framework requires. A refused one (no slot, or the descriptor
    // could not be duplicated) is never retained.
    unsafe impl VZVirtioSocketListenerDelegate for HelperDelegate {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> bool {
            self.accept_egress(connection)
        }
    }
);

impl HelperDelegate {
    /// Hand `connection` to the egress bridges with a slot, or refuse it
    /// while the bound is reached. A connection not handed on drops its
    /// slot with it.
    fn accept_egress(&self, connection: &VZVirtioSocketConnection) -> bool {
        let channels = self.ivars();
        let Some(slot) = channels.slots.try_take() else {
            let text = "egress connection refused: the bridge bound is reached";
            if let Some(line) = channels.egress_diag.line(Instant::now(), text) {
                diag(&line);
            }
            return false;
        };
        match VsockConn::adopt(connection) {
            Some(conn) => channels.egress.send((conn, slot)).is_ok(),
            None => false,
        }
    }

    pub(crate) fn new(channels: Channels) -> Retained<Self> {
        let this = Self::alloc().set_ivars(channels);
        // SAFETY: NSObject's `init` on a freshly allocated object whose
        // ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}
