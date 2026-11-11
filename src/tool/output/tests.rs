use super::*;
use crate::tool::test_support::TempDir;

#[test]
fn a_huge_unicode_line_shows_an_exact_prefix_and_saves_the_whole_capture() {
    let dir = TempDir::new("output-clip");
    let budget = 16 * 1024 + 3;
    let mut collector = Collector::new(
        budget,
        &Storage::open(dir.path()).unwrap(),
        crate::tool::output::Name::Output(2),
    )
    .unwrap();
    let line = "😀".repeat(16 * 1024);
    collector.line(&line);
    let path = collector.path().to_path_buf();
    let (text, notes, complete) = collector.finish();
    assert!(complete.is_complete());
    assert!(
        text.len() < budget + 200,
        "{} bytes under a {budget}-byte budget",
        text.len()
    );
    assert_eq!(text, line[..16 * 1024]);
    assert_eq!(
        notes,
        "[Bart output: first 16384 of 65537 bytes shown; all of it: cell[2].output]\n"
    );
    assert!(!text.contains("Bart output:"));
    assert_eq!(
        render("output", "\nfirst note\n\nsecond note\n"),
        "output\n\nfirst note\n\nsecond note"
    );
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        format!("{line}\n"),
        "the file keeps it whole"
    );
}

#[test]
fn one_file_stops_at_its_limit_and_says_so() {
    let dir = TempDir::new("output-file-cap");
    let mut collector = Collector::new(
        100,
        &Storage::open(dir.path()).unwrap(),
        crate::tool::output::Name::Output(3),
    )
    .unwrap();
    let line = "x".repeat(1024 * 1024 - 1);
    for _ in 0..70 {
        collector.line(&line);
    }
    collector.line("IMPORTANT_TAIL");
    let path = collector.path().to_path_buf();
    let (text, notes, complete) = collector.finish();
    assert!(!complete.is_complete());
    assert!(
        !text.contains("IMPORTANT_TAIL"),
        "no synthetic tail is shown"
    );
    assert!(
        notes.contains("cell[3].output, incomplete: file limit reached (64 MiB)"),
        "{notes}"
    );

    assert_eq!(fs::metadata(path).unwrap().len(), MAX_FILE_BYTES as u64);
}

#[test]
fn numbers_continue_after_the_highest_saved_file_of_each_kind() {
    let dir = TempDir::new("output-numbers");
    let storage = Storage::open(dir.path()).unwrap();
    let cell = storage.next_cell().unwrap();
    save(&storage, Name::Code(cell), b"x").unwrap();
    save(&storage, Name::Output(storage.next_cell().unwrap()), b"y").unwrap();
    save(
        &storage,
        Name::Image {
            number: storage.next_image().unwrap(),
            extension: "png",
        },
        b"z",
    )
    .unwrap();
    save(&storage, Name::Job(7), b"w").unwrap();
    assert!(
        save(&storage, Name::Code(cell), b"again").is_err(),
        "a saved file is never replaced"
    );
    let reopened = Storage::open(dir.path()).unwrap();
    assert_eq!(
        (
            reopened.next_cell().unwrap(),
            reopened.next_image().unwrap(),
            reopened.highest_job()
        ),
        (3, 2, 7)
    );
}

#[test]
fn exhausted_numbers_refuse_instead_of_wrapping() {
    let dir = TempDir::new("output-exhausted");
    fs::write(dir.path().join(format!("code-{}.py", u64::MAX)), "x").unwrap();
    fs::write(dir.path().join(format!("image-{}.png", u64::MAX)), "x").unwrap();
    let storage = Storage::open(dir.path()).unwrap();
    assert!(storage.next_cell().is_err());
    assert!(storage.next_cell().is_err(), "the counter stays exhausted");
    assert!(storage.next_image().is_err());
}

#[tokio::test]
async fn reopening_an_incomplete_output_keeps_it_incomplete_and_counts_from_its_start() {
    let dir = TempDir::new("output-reopen");
    let filler = fs::File::create(dir.path().join("filler")).unwrap();
    filler.set_len(MAX_SESSION_BYTES - 2000).unwrap();
    let storage = Storage::open(dir.path()).unwrap();
    let mut first = Collector::new(100, &storage, Name::Output(1)).unwrap();
    first.line("EARLY");
    first.line(&"m".repeat(3500));
    let earlier = first.saved();
    assert!(!earlier.is_complete());
    let (_, _, finished) = first.finish();
    assert_eq!(finished.chars, earlier.chars);
    let mut more = Collector::reopen(100, &storage, Name::Output(1), earlier.clone())
        .await
        .unwrap();
    more.line(&"z".repeat(800));
    let (text, notes, saved) = more.finish();
    assert!(
        !saved.is_complete(),
        "a reopened output with a hole stays incomplete"
    );
    assert_eq!(saved.chars, earlier.chars + 801);
    assert_eq!(text, "z".repeat(100));
    assert!(notes.contains("first 100 of 801 new bytes shown"));
    assert!(notes.contains("cell[1].output, incomplete"), "{notes}");
    assert!(
        !fs::read_to_string(dir.path().join("output-1.log"))
            .unwrap()
            .contains('z'),
        "nothing is saved after the hole"
    );
}

#[tokio::test]
async fn raw_stream_artifacts_obey_quotas_and_report_capture_and_save_failures() {
    for failure in ["file", "session", "create", "capture"] {
        let dir = TempDir::new("raw-output-bounds");
        let storage = Storage::open(dir.path()).unwrap();
        let mut collector =
            Collector::new(100, &storage, Name::Process { cell: 1, call: 1 }).unwrap();
        if failure == "create" {
            fs::write(dir.path().join("run-1-1.stdout"), b"existing").unwrap();
        }
        collector.start_streams().await;
        match failure {
            "file" => {
                let bytes = vec![b'x'; 64 * 1024];
                for _ in 0..=MAX_FILE_BYTES / bytes.len() {
                    collector.stream_bytes(0, &bytes);
                    collector.flush().await;
                }
            }
            "session" => {
                assert!(storage.reserve(MAX_SESSION_BYTES, 0));
                collector.stream_bytes(0, b"not admitted");
            }
            "create" => collector.stream_bytes(0, b"must not overwrite"),
            "capture" => {
                collector.stream_bytes(0, b"prefix");
                collector.capture_failed(CaptureFailure::DrainDeadline);
            }
            _ => unreachable!(),
        }
        let [stdout, _] = collector.finish_streams([true; 2]).unwrap();
        assert!(stdout.incomplete.is_some(), "{failure}: {stdout:?}");
        let saved = fs::read(stdout.path).unwrap();
        match failure {
            "file" => assert_eq!(saved.len(), MAX_FILE_BYTES),
            "session" => assert!(saved.is_empty()),
            "create" => assert_eq!(saved, b"existing"),
            "capture" => assert_eq!(saved, b"prefix"),
            _ => unreachable!(),
        }
    }
}

#[test]
fn multiline_prefix_never_resumes_after_an_omitted_unicode_character() {
    let dir = TempDir::new("output-prefix-unicode");
    let mut collector =
        Collector::new(5, &Storage::open(dir.path()).unwrap(), Name::Output(1)).unwrap();
    collector.line("abc");
    collector.line("😀rest");
    collector.line("TAIL");
    let (text, notes, saved) = collector.finish();
    assert_eq!(text, "abc\n");
    assert!(notes.contains("first 4 of 18 bytes shown"));
    assert!(saved.is_complete());
    assert_eq!(
        fs::read_to_string(saved.path).unwrap(),
        "abc\n😀rest\nTAIL\n"
    );
}

#[test]
fn display_omission_and_incomplete_capture_are_distinct_and_not_saved_as_source() {
    for failure in [
        CaptureFailure::DrainDeadline,
        CaptureFailure::Read(io::Error::other("lost pipe")),
    ] {
        let dir = TempDir::new("output-capture-prefix");
        let mut collector =
            Collector::new(4, &Storage::open(dir.path()).unwrap(), Name::Output(1)).unwrap();
        collector.line("captured");
        collector.capture_failed(failure);
        let (text, notes, saved) = collector.finish();
        assert_eq!(text, "capt");
        assert!(!saved.is_complete());
        assert!(notes.contains("first 4 of 9 bytes shown"));
        assert!(notes.contains("saved copy (incomplete):"));
        assert!(notes.contains("total output size unknown"));
        assert_eq!(fs::read_to_string(saved.path).unwrap(), "captured\n");
    }
}

#[test]
fn short_and_exact_budget_output_need_no_delivery_notice() {
    for budget in [6, 20] {
        let dir = TempDir::new("output-prefix-short");
        let mut collector =
            Collector::new(budget, &Storage::open(dir.path()).unwrap(), Name::Output(1)).unwrap();
        collector.line("hello");
        let (text, notes, saved) = collector.finish();
        assert_eq!(text, "hello\n");
        assert!(notes.is_empty());
        assert!(saved.is_complete());
    }
}
