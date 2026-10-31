use super::*;
use crate::tool::test_support::TempDir;

#[test]
fn explicit_prefix_loads_bounded_text_without_interpreting_metadata() {
    let dir = TempDir::new("skills");
    let folder = dir.path().join("review");
    std::fs::create_dir(&folder).unwrap();
    let file = folder.join("SKILL.md");
    let body =
        "---\nname: review\nmodel: not-a-runtime-override\n---\nReview the requested change.\n";
    std::fs::write(&file, body).unwrap();
    let skill = load(dir.path(), "review").unwrap().unwrap();
    assert!(skill.text.ends_with(body));
    std::fs::write(&file, "changed").unwrap();
    assert!(skill.text.ends_with(body));
    assert!(resolve("Discuss /review here").unwrap().is_none());
    assert!(resolve(" /review").unwrap().is_none());
    for text in [
        "/tmp/x has the results",
        "/",
        "//host/path",
        "/../review please",
    ] {
        assert!(resolve(text).unwrap().is_none(), "{text:?}");
        let mut input = crate::agent::Input::from(text.to_owned());
        input.resolve_skill().unwrap();
        assert_eq!(input.text, text);
        assert!(input.skills.is_empty());
    }
    for name in [
        "",
        ".",
        "..",
        "../review",
        "review/other",
        "review/.",
        "review//",
        "review\\other",
        "review\u{1b}",
    ] {
        assert!(load(dir.path(), name).unwrap().is_none(), "{name:?}");
    }
    for bytes in [
        Vec::new(),
        vec![255],
        vec![b'x'; usize::try_from(MAX_BYTES).unwrap() + 1],
    ] {
        std::fs::write(&file, bytes).unwrap();
        assert!(load(dir.path(), "review").is_err());
    }
    std::fs::remove_file(&file).unwrap();
    assert!(load(dir.path(), "review").unwrap().is_none());
    assert!(load(dir.path(), "unknown").unwrap().is_none());
    std::os::unix::fs::symlink("missing", &file).unwrap();
    assert!(load(dir.path(), "review").unwrap().is_none());
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink("/dev/null", &file).unwrap();
    assert!(load(dir.path(), "review").is_err());
}
