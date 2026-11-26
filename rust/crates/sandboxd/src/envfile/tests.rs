use super::*;

#[test]
fn parses_keys_values_comments_and_overrides() {
    let text = "# image environment\n\
                PATH=/usr/local/bin:/usr/bin:/bin\r\n\
                \n\
                BASH_ENV=/etc/sandbox/bashrc\n\
                HTTPS_PROXY=http://127.0.0.1:3128\n\
                EMPTY=\n\
                EQUALS=a=b\n\
                PATH=/bin\n";
    let (vars, skipped) = parse(text);
    assert_eq!(skipped, 0);
    let expected = [
        ("PATH", "/bin"),
        ("BASH_ENV", "/etc/sandbox/bashrc"),
        ("HTTPS_PROXY", "http://127.0.0.1:3128"),
        ("EMPTY", ""),
        ("EQUALS", "a=b"),
    ];
    let got: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(got, expected);
}

#[test]
fn malformed_lines_are_counted_and_skipped() {
    let text = "no equals sign\n1BAD=x\nexport X=1\nGOOD_1=ok\n=empty key\nNUL=a\0b\n";
    let (vars, skipped) = parse(text);
    assert_eq!(vars, [("GOOD_1".to_owned(), "ok".to_owned())]);
    assert_eq!(skipped, 5);
}

#[test]
fn a_missing_file_is_an_empty_environment() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load(&dir.path().join("absent")).is_empty());
    std::fs::write(dir.path().join("env"), "A=1\n").unwrap();
    assert_eq!(
        load(&dir.path().join("env")),
        [("A".to_owned(), "1".to_owned())]
    );
}
