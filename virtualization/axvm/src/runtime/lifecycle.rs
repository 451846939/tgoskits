//! Host application callbacks for VM runtime lifecycle transitions.

use core::sync::atomic::{AtomicPtr, Ordering};

static VM_STARTED: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static VM_STOPPING: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Registers host application callbacks for VM start and stop transitions.
///
/// Registration is expected during VMM initialization, before any VM starts.
/// Re-registering replaces both callbacks atomically from the perspective of
/// subsequent lifecycle notifications.
pub fn register_hooks(on_started: fn(usize), on_stopping: fn(usize)) {
    VM_STOPPING.store(on_stopping as *mut (), Ordering::Release);
    VM_STARTED.store(on_started as *mut (), Ordering::Release);
}

pub(crate) fn notify_started(vm_id: usize) {
    call_hook(&VM_STARTED, vm_id);
}

pub(crate) fn notify_stopping(vm_id: usize) {
    call_hook(&VM_STOPPING, vm_id);
}

fn call_hook(hook: &AtomicPtr<()>, vm_id: usize) {
    let callback = hook.load(Ordering::Acquire);
    if callback.is_null() {
        return;
    }

    // SAFETY: `register_hooks` only stores function pointers with this exact
    // signature, and the pointer remains valid for the lifetime of the kernel.
    let callback = unsafe { core::mem::transmute::<*mut (), fn(usize)>(callback) };
    callback(vm_id);
}
