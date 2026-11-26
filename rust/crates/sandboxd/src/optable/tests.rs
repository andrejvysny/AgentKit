use super::*;

fn hash(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

fn new_entry(table: &OpTable, op: &str, args: &str) -> Arc<OpEntry> {
    match table.admit(op, args, OpKind::Exec(ExecOp::default())) {
        Admission::New(entry) => entry,
        _ => panic!("{op} should be new"),
    }
}

#[test]
fn same_id_same_hash_is_existing_other_hash_conflicts() {
    let table = OpTable::default();
    let entry = new_entry(&table, "op-1", &hash(1));
    assert_eq!(entry.state(), OperationState::Accepted);
    assert!(matches!(
        table.admit("op-1", &hash(1), OpKind::Write),
        Admission::Existing(OperationState::Accepted)
    ));
    assert!(matches!(
        table.admit("op-1", &hash(2), OpKind::Write),
        Admission::Conflict
    ));
    entry.mark_running();
    entry.finish(OperationState::Succeeded { exit: 0 });
    assert!(matches!(
        table.admit("op-1", &hash(1), OpKind::Write),
        Admission::Existing(OperationState::Succeeded { exit: 0 })
    ));
    assert!(matches!(
        table.admit("op-1", &hash(2), OpKind::Write),
        Admission::Conflict
    ));
}

#[test]
fn unknown_ids_are_absent() {
    let table = OpTable::default();
    new_entry(&table, "op-1", &hash(1));
    assert!(table.get("op-1").is_some());
    assert!(table.get("never").is_none());
}

#[test]
fn the_first_terminal_state_is_the_receipt() {
    let table = OpTable::default();
    let entry = new_entry(&table, "op-1", &hash(1));
    entry.mark_running();
    assert_eq!(entry.state(), OperationState::Running);
    entry.finish(OperationState::Cancelled);
    entry.finish(OperationState::Succeeded { exit: 0 });
    entry.finish(failed(1));
    entry.mark_running();
    assert_eq!(entry.state(), OperationState::Cancelled);
}

#[test]
fn mark_running_only_moves_accepted() {
    let table = OpTable::default();
    let entry = new_entry(&table, "op-1", &hash(1));
    entry.finish(failed(126));
    entry.mark_running();
    assert_eq!(entry.state(), failed(126));
}

#[test]
fn exec_open_tracks_commands_without_receipt() {
    let table = OpTable::default();
    assert!(!table.exec_open());
    let write = match table.admit("w", &hash(3), OpKind::Write) {
        Admission::New(entry) => entry,
        _ => panic!("new"),
    };
    assert!(!table.exec_open(), "writes are not commands");
    assert!(write.exec().is_none());
    let exec = new_entry(&table, "e", &hash(4));
    assert!(table.exec_open());
    exec.finish(OperationState::TimedOut);
    assert!(!table.exec_open());
}

#[test]
fn terminal_states_are_classified() {
    for state in [
        OperationState::Succeeded { exit: 0 },
        failed(3),
        OperationState::TimedOut,
        OperationState::Cancelled,
        OperationState::OutputLimit,
    ] {
        assert!(is_terminal(state), "{state:?}");
    }
    for state in [
        OperationState::Accepted,
        OperationState::Running,
        OperationState::Unknown,
    ] {
        assert!(!is_terminal(state), "{state:?}");
    }
}
