use super::*;

#[test]
fn a_handle_from_the_wire_is_parsed_strictly() {
    assert!(serde_json::from_str::<OutputHandle>("\"../x\"").is_err());
    assert!(serde_json::from_str::<OutputHandle>("\"out_../../etc/passwd\"").is_err());
    let handle = OutputHandle::generate();
    let wire = serde_json::to_string(&handle).unwrap();
    assert_eq!(serde_json::from_str::<OutputHandle>(&wire).unwrap(), handle);
}

#[test]
fn lines_end_at_cr_lf_and_crlf() {
    assert_eq!(count_lines(b"a\rb\r\nc\nd"), 4);
    assert_eq!(count_lines(b"a\r\n"), 1);
    assert_eq!(count_lines(b""), 0);
    let slice = slice_bytes(
        b"one\rtwo\r\nthree",
        ByteOrLineRange::Lines {
            start_line: 2,
            max_lines: 1,
        },
    );
    assert_eq!(
        (slice.text.as_str(), slice.total_lines, slice.has_more),
        ("two", 3, true)
    );
}

#[test]
fn a_quote_heavy_preview_fits_once_escaped() {
    let raw = "\"q\" \\ \"quoted\" text\n".repeat(5_000);
    let handle = OutputHandle::generate();
    for limit in [1024, 4096, 8192] {
        let preview = render_preview(&raw, &handle, false, limit, None);
        assert!(
            escaped_len(&preview) <= limit,
            "{limit}: {}",
            escaped_len(&preview)
        );
        assert!(preview.ends_with("search_output(handle, pattern)]"));
        assert!(preview.contains(handle.as_str()));
    }
}

#[test]
fn a_line_longer_than_the_budget_is_marked_cut() {
    let raw = "x".repeat(50_000);
    let handle = OutputHandle::generate();
    let preview = render_preview(&raw, &handle, false, 8192, None);
    assert!(preview.contains("[line cut]"));
    let capped = render_preview(&raw, &handle, false, 8192, Some(70_000_000));
    assert!(capped.contains("truncated at the 64 MiB storage cap"));
}
