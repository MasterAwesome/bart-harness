use super::*;
use crate::history::{Completeness, InlineImage, SavedArtifact, test_support::PNG};

#[tokio::test]
async fn delivered_text_fits_the_budget_whatever_is_added_late() {
    let dir = test_support::TempDir::new("deliver-fit");
    let context = test_support::context(dir.path(), Confinement::Unrestricted, 1000);
    let mut output = Output::error("界".repeat(2000));
    output.images = vec![InlineImage::new(PNG.to_vec(), None, "plot".into()).unwrap(); 3];
    let path = context.artifacts.dir().join("output-4.log");
    std::fs::write(&path, &output.text).unwrap();
    output.saved = Some(SavedArtifact {
        path,
        completeness: Completeness::Complete,
        chars: output.text.chars().count(),
        bytes: output.text.len(),
    });
    output.cell = Some(4);
    let images = deliver(&mut output, &context, 0, false).await;
    assert_eq!(images.len(), 0);
    assert!(
        output.text.len()
            <= 1000 + output::HOST_METADATA_BYTES + output::HOST_METADATA_BOUNDARY.len(),
        "{} bytes",
        output.text.len()
    );
    assert_eq!(&output.text[..999], "界".repeat(333));
    assert!(output.text[999..].starts_with(output::HOST_METADATA_BOUNDARY));
    assert!(output.notes.contains("cell[4] 999 of 6000 bytes shown"));
    assert!(
        output.text.contains("rest: cell[4].remaining()"),
        "the full output stays reachable"
    );
    assert!(
        output.text.ends_with("cannot see images]"),
        "{}",
        output.text
    );
    assert_eq!(output.status, Status::Failed);
}

#[tokio::test]
async fn metadata_is_bounded_separate_and_delivery_is_idempotent() {
    let dir = test_support::TempDir::new("deliver-metadata");
    let context = test_support::context(dir.path(), Confinement::Unrestricted, 32 * 1024);
    let source = format!("{}TAIL", "α".repeat(20_000));
    let mut result = Output::success(source.clone());
    result.notes = (0..5000).map(|n| format!("record {n}\n")).collect();
    deliver(&mut result, &context, 0, true).await;
    assert_eq!(&result.text[..32768], &source[..32768]);
    assert!(!result.text.contains("TAIL"));
    assert!(result.text.contains("Bart output: further notes omitted"));
    assert!(
        result.text.len()
            <= 32768 + output::HOST_METADATA_BOUNDARY.len() + output::HOST_METADATA_BYTES
    );
    let rendered = result.text.clone();
    deliver(&mut result, &context, 0, true).await;
    assert_eq!(result.text, rendered);
}
