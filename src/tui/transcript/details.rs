use super::*;

impl Transcript {
    pub(super) fn start_compaction(&mut self) {
        let sequence = self.next_detail.take_next();
        self.details.push(Detail {
            filter: Default::default(),
            kind: DetailKind::Compaction { running: true },
            sequence,
            live_run_id: None,
            order_hint: self
                .details
                .last()
                .and_then(|d| d.order_hint.or(d.live_run_id)),
            title: Clean::literal("Compaction · waiting"),
            review: None,
            code: Clean::default(),
            output: Clean::default(),
            highlights: Vec::new(),
        });
        self.compaction = Some(sequence);
        let mut held: usize = self.details.iter().map(Detail::bytes).sum();
        let mut removed = 0;
        while removed + 1 < self.details.len()
            && (self.details.len() - removed > MAX_DETAILS
                || held + crate::detail_excerpt::OUTPUT_BYTES > MAX_DETAIL_BYTES)
        {
            held -= self.details[removed].bytes();
            removed += 1;
        }
        self.details.drain(..removed);
    }

    pub(super) fn compaction_detail(&mut self) -> Option<&mut Detail> {
        let sequence = self.compaction?;
        self.details
            .iter_mut()
            .rev()
            .find(|detail| detail.sequence == sequence)
    }

    pub(super) fn finish_compaction(&mut self, status: &str) {
        if let Some(detail) = self.compaction_detail() {
            detail.title = if status == "completed" {
                Clean::literal("Compaction summary")
            } else {
                Clean::new(&format!("Compaction · {status}"))
            };
            if let DetailKind::Compaction { running, .. } = &mut detail.kind {
                *running = false;
            }
        }
        self.compaction = None;
    }

    pub(in crate::tui) fn compaction_note(&mut self, output: &str) {
        self.note("Compaction summary", output);
        if let Some(detail) = self.details.last_mut() {
            detail.kind = DetailKind::Compaction { running: false };
        }
    }

    pub(in crate::tui) fn note(&mut self, title: &str, output: &str) {
        self.details.push(Detail {
            filter: Default::default(),
            kind: DetailKind::Note,
            sequence: self.next_detail.take_next(),
            live_run_id: None,
            order_hint: None,
            title: Clean::new(title),
            review: None,
            code: Clean::default(),
            output: Clean::new(output),
            highlights: Vec::new(),
        });
        self.bound_details();
    }

    pub(in crate::tui) fn ui_notice(&mut self, kind: StatusKind, body: &Clean) {
        const LIMIT: usize = 8192;
        const TRUNCATED: &str = "\n… notice body truncated";
        let truncated = body.len() > LIMIT;
        let output = if truncated {
            Clean::new(&format!(
                "{}{TRUNCATED}",
                &body[..body.floor_char_boundary(LIMIT - TRUNCATED.len())]
            ))
        } else {
            body.clone()
        };
        if let Some(detail) = self.details.last_mut()
            && let DetailKind::UiNotice {
                kind: previous,
                repeats,
                truncated: false,
                ..
            } = &mut detail.kind
            && *previous == kind
            && !truncated
            && detail.output == output
        {
            *repeats = repeats.saturating_add(1);
            detail.refresh_ui_title();
            self.label_ui_omission();
            return;
        }
        if self
            .details
            .iter()
            .filter(|d| matches!(d.kind, DetailKind::UiNotice { .. }))
            .count()
            >= 32
            && let Some(index) = self
                .details
                .iter()
                .position(|d| matches!(d.kind, DetailKind::UiNotice { .. }))
        {
            self.details.remove(index);
            self.ui_notices_omitted = true;
        }
        let order_hint = self
            .details
            .iter()
            .rev()
            .find_map(|detail| detail.order_hint.or(detail.live_run_id));
        self.details.push(Detail {
            filter: Default::default(),
            kind: DetailKind::UiNotice {
                kind,
                repeats: 1,
                truncated,
                older_omitted: false,
            },
            sequence: self.next_detail.take_next(),
            live_run_id: None,
            order_hint,
            title: Clean::literal("UI notice"),
            review: None,
            code: Clean::default(),
            output,
            highlights: Vec::new(),
        });
        self.bound_details();
        self.label_ui_omission();
    }

    fn label_ui_omission(&mut self) {
        if self.ui_notices_omitted
            && let Some(detail) = self
                .details
                .iter_mut()
                .find(|d| matches!(d.kind, DetailKind::UiNotice { .. }))
            && let DetailKind::UiNotice { older_omitted, .. } = &mut detail.kind
        {
            *older_omitted = true;
            detail.refresh_ui_title();
        }
    }

    pub(super) fn bound_details(&mut self) {
        let mut held: usize = self.details.iter().map(Detail::bytes).sum();
        let mut removed = 0;
        while self.details.len() - removed > MAX_DETAILS || held > MAX_DETAIL_BYTES {
            held -= self.details[removed].bytes();
            removed += 1;
        }
        if removed > 0 {
            self.ui_notices_omitted |= self.details[..removed]
                .iter()
                .any(|d| matches!(d.kind, DetailKind::UiNotice { .. }));
            self.details.drain(..removed);
            self.details.shrink_to_fit();
            self.label_ui_omission();
        }
    }
}
