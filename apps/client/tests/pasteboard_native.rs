//! Touches the user's real clipboard (text content is saved and restored), so it only
//! runs on request: `cargo test -p binachicken-client --test pasteboard_native -- --ignored`.
#![cfg(target_os = "macos")]
use binachicken_client::clipboard::{Content, MacPasteboard, Pasteboard};

#[test]
#[ignore = "modifies the real macOS pasteboard"]
fn text_and_file_urls_round_trip_through_the_real_pasteboard() {
    let mut board = MacPasteboard;
    let saved = match board.read() {
        Some(Content::Text(t)) => Some(t),
        _ => None,
    };
    let before = board.change_count();
    board.write_text("xin chào 😀\nline2").unwrap();
    assert!(board.change_count() > before);
    assert_eq!(
        board.read(),
        Some(Content::Text("xin chào 😀\nline2".into()))
    );

    let dir = std::env::temp_dir().join(format!("bc-pb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let files = [dir.join("báo cáo.txt"), dir.join("b.bin")];
    for f in &files {
        std::fs::write(f, b"x").unwrap();
    }
    board.write_files(&files).unwrap();
    let Some(Content::Files(read)) = board.read() else {
        panic!("expected files")
    };
    let canon = |p: &std::path::PathBuf| std::fs::canonicalize(p).unwrap();
    assert_eq!(
        read.iter().map(canon).collect::<Vec<_>>(),
        files.iter().map(canon).collect::<Vec<_>>()
    );

    match saved {
        Some(t) => board.write_text(&t).unwrap(),
        None => board.write_text("").unwrap(),
    }
}
