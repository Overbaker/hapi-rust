pub fn format_message(text: &str, paths: &[String]) -> String {
    let refs = paths
        .iter()
        .map(|p| format!("@{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    if refs.is_empty() {
        text.into()
    } else if text.is_empty() {
        refs
    } else {
        format!("{refs}\n\n{text}")
    }
}
#[test]
fn attachment_references() {
    assert_eq!(
        format_message("read", &["a b".into(), "c".into()]),
        "@a b @c\n\nread"
    );
    assert_eq!(format_message("", &["a".into()]), "@a");
    assert_eq!(format_message("text", &[]), "text");
}
