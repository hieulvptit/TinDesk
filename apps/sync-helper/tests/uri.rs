use binachicken_sync_helper::uri::*;
use std::path::PathBuf;

#[test]
fn round_trips_hostile_and_unicode_names() {
    for name in [
        "/tmp/a b.txt",
        "/tmp/báo cáo #1 (100%).txt",
        "/tmp/a\nb",
        "/tmp/😀",
        "/tmp/x?y=z&w",
    ] {
        let p = PathBuf::from(name);
        assert_eq!(
            parse_list(&uri_list(std::slice::from_ref(&p))),
            vec![p.clone()],
            "{name:?}"
        );
        assert!(
            !uri_list(&[p]).trim_end().contains('\n'),
            "newline must be escaped"
        );
    }
}

#[test]
fn gnome_format_skips_operation_line_and_parses_back() {
    let paths = vec![PathBuf::from("/a/b"), PathBuf::from("/c d")];
    let body = gnome_copied(&paths);
    assert!(body.starts_with("copy\nfile:///a/b"));
    assert!(!body.ends_with('\n'));
    assert_eq!(parse_any(&body), paths);
}

#[test]
fn rejects_remote_relative_and_nul_entries() {
    let text = "file://evil.example/etc/passwd\nfile:relative\nhttp://x/y\nfile:///ok\nfile:///nul%00byte\nfile:///bad%zz\n# comment\n";
    assert_eq!(parse_list(text), vec![PathBuf::from("/ok")]);
    assert_eq!(
        parse_list("file://localhost/etc/hosts"),
        vec![PathBuf::from("/etc/hosts")]
    );
}
