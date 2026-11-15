//! T0.3: the run record's outcome rules as public API, shared with the
//! conformance runner.
use agentkit_contracts::AiToolOutcomeClass as Class;
use agentkit_core::{CallFailure, OutcomeClassifier};

#[test]
fn codes_and_known_classes_follow_the_same_repair_and_repeat_rules() {
    let mut classifier = OutcomeClassifier::new();
    let bad = Some(CallFailure::Code {
        code: "schema_invalid",
        message: "",
    });
    assert_eq!(
        classifier.judge("edit", "{}", bad).class,
        Class::InvalidStructure
    );
    let second = classifier.judge("edit", "{}", bad);
    assert_eq!(second.class, Class::InvalidStructure);
    assert!(
        second.stop_retrying,
        "the second failure in a class stops retrying"
    );
    assert_eq!(
        classifier.judge("edit", "{}", bad).class,
        Class::RepeatIdentical
    );
    assert_eq!(
        classifier.judge("edit", "{\"a\":1}", None).class,
        Class::OkAfterRepair
    );
    // A caller that already knows the class gets the same counters.
    let mut harness = OutcomeClassifier::new();
    let precondition = Some(CallFailure::Class(Class::Precondition));
    assert_eq!(
        harness.judge("read", "{}", precondition).class,
        Class::Precondition
    );
    assert_eq!(
        harness.judge("read", "{}", precondition).class,
        Class::Precondition
    );
    assert_eq!(
        harness.judge("read", "{}", precondition).class,
        Class::RepeatIdentical
    );
    assert_eq!(harness.judge("write", "{}", None).class, Class::Ok);
    // A code outside the budget (a cancel) does not extend a streak.
    let cancel = Some(CallFailure::Code {
        code: "cancelled",
        message: "",
    });
    let verdict = OutcomeClassifier::new().judge("run", "{}", cancel);
    assert!(!verdict.stop_retrying);
}
