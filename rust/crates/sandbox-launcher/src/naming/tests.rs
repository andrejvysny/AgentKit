use super::*;

#[test]
fn workspace_ids_are_checked_before_any_name() {
    for bad in [
        "",
        "../x",
        "a/b",
        "UPPER",
        "a b",
        "a.b",
        "a_b",
        &"a".repeat(65),
    ] {
        let error = WorkspaceId::parse(bad).unwrap_err();
        assert!(matches!(error, ServiceError::Invalid(_)), "{bad:?}");
    }
    let ws = WorkspaceId::parse("019a7c3e-ws").unwrap();
    assert_eq!(ws.as_str(), "019a7c3e-ws");
    assert!(WorkspaceId::parse(&"a".repeat(64)).is_ok());
}

#[test]
fn generations_start_at_one() {
    assert!(check_generation(0).is_err());
    assert_eq!(check_generation(1).unwrap(), 1);
    assert_eq!(check_generation(u64::MAX).unwrap(), u64::MAX);
}

#[test]
fn generation_labels_must_be_canonical() {
    assert_eq!(parse_generation_label("7"), Some(7));
    assert_eq!(
        parse_generation_label("18446744073709551615"),
        Some(u64::MAX)
    );
    for bad in [
        "0",
        "07",
        "+7",
        " 7",
        "7 ",
        "-1",
        "",
        "x",
        "18446744073709551616",
    ] {
        assert_eq!(parse_generation_label(bad), None, "{bad:?}");
    }
}

#[test]
fn names_follow_the_template() {
    let ws = WorkspaceId::parse("ws-1").unwrap();
    let names = Names::new("aks-");
    assert_eq!(names.container(&ws, 3), "aks-ws-1-g3");
    assert_eq!(names.workspace_volume(&ws), "aks-ws-ws-1");
    assert_eq!(names.home_volume(&ws), "aks-home-ws-1");
    assert_eq!(workspace_filter(&ws), "agentkit.sandbox.workspace=ws-1");
    assert_eq!(managed_filter(), "agentkit.sandbox.managed=1");
    let labels = container_labels(&ws, 3, "sha256:aa");
    assert_eq!(labels[LABEL_GENERATION], "3");
    assert_eq!(labels[LABEL_IMAGE], "sha256:aa");
    assert_eq!(labels[LABEL_MANAGED], "1");
    assert_eq!(volume_labels(&ws, "home")[LABEL_VOLUME], "home");
}
