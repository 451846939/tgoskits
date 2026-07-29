#[path = "../src/consts.rs"]
mod consts;

use consts::traps::irq::{S_EXT, S_EXT_CODE, is_supervisor_external};

#[test]
fn supervisor_external_interrupt_accepts_runtime_id_and_scause_encoding() {
    assert!(is_supervisor_external(S_EXT_CODE));
    assert!(is_supervisor_external(S_EXT));
    assert!(!is_supervisor_external(5));
}
