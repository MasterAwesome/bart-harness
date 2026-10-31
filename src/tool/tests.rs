use super::*;
use crate::history::{Completeness, InlineImage, SavedArtifact, test_support::PNG};

#[tokio::test]
async fn delivered_text_fits_the_budget_whatever_is_added_late() {
    let dir = test_support::TempDir::new("deliver-fit");
    let context = test_support::context(dir.path(), Confinement::Unrestricted, 1000);
    let mut output = Output::error("界".repeat(2000));
    output.images = vec![InlineImage::new(PNG.to_vec(), None, "plot".into()).unwrap(); 3];
    output.saved = Some(SavedArtifact {
        path: dir.path().join("cell.log"),
        completeness: Completeness::Complete,
        chars: 0,
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
    assert!(output.notes.contains("999/6000 bytes shown"));
    assert!(
        output.text.contains("full output: cell[4].output"),
        "the full output stays reachable"
    );
    assert!(output.text.ends_with("none was shown]"), "{}", output.text);
    assert_eq!(output.status, Status::Failed);
}

#[tokio::test]
async fn delivered_failures_identify_their_cell_without_taxing_successful_output() {
    let dir = test_support::TempDir::new("deliver-cell-id");
    let context = test_support::context(dir.path(), Confinement::Unrestricted, 1000);
    for status in [Status::Succeeded, Status::Failed] {
        let mut output = Output::error("brief".into());
        output.status = status;
        output.cell = Some(17);
        deliver(&mut output, &context, 0, false).await;
        if status == Status::Failed {
            assert_eq!(
                output.text,
                format!("brief{}[cell 17]", output::HOST_METADATA_BOUNDARY)
            );
            assert!(output.notes.starts_with("[cell 17]"));
        } else {
            assert_eq!(output.text, "brief");
            assert!(output.notes.is_empty());
        }
        assert_eq!(output.status, status);
    }
    let mut not_run = Output::error("not run".into());
    deliver(&mut not_run, &context, 0, false).await;
    assert_eq!(not_run.text, "not run");
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
    assert!(result.text.contains("Bart metadata: remainder omitted"));
    assert!(
        result.text.len()
            <= 32768 + output::HOST_METADATA_BOUNDARY.len() + output::HOST_METADATA_BYTES
    );
    let rendered = result.text.clone();
    deliver(&mut result, &context, 0, true).await;
    assert_eq!(result.text, rendered);
}
