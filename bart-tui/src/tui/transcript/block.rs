use super::*;

pub(in crate::tui) struct Block {
    pub(super) kind: Kind,
    pub(in crate::tui) rows: Arc<Vec<RenderRow>>,
}

pub(super) enum Kind {
    User(Arc<Clean>),
    Prose(u64),
    Reasoning,
    Tools(Vec<Call>),
    Diff,
    Notice,
    Compaction(Arc<Clean>),
}

impl Block {
    pub(super) fn new(kind: Kind, text: impl Into<RenderText>) -> Self {
        Self {
            kind,
            rows: Arc::new(text.into().lines),
        }
    }

    pub(super) fn source(&self) -> CopySource {
        match &self.kind {
            Kind::User(source) | Kind::Compaction(source) => CopySource::Text(source.clone()),
            _ => CopySource::Rows(self.rows.clone()),
        }
    }

    pub(in crate::tui) fn rows(
        &self,
        width: u16,
        ui: crate::preferences::Ui,
    ) -> std::borrow::Cow<'_, [RenderRow]> {
        if let Kind::Tools(calls) = &self.kind
            && !calls.is_empty()
        {
            let mut rows = Vec::new();
            if self.rows.first().is_some_and(RenderRow::is_blank) {
                rows.push(RenderRow::Literal(Line::default()));
            }
            let lines = if ui == crate::preferences::Ui::Cells {
                tools(calls, None, width, 0)
            } else {
                tools::activities(calls, None, width, 0)
            };
            rows.extend(lines.into_iter().map(RenderRow::Literal));
            return rows.into();
        }
        std::borrow::Cow::Borrowed(&self.rows)
    }
    pub(super) fn bytes(&self) -> usize {
        let extra: usize = match &self.kind {
            Kind::User(source) | Kind::Compaction(source) => source.len(),
            Kind::Tools(calls) => calls
                .iter()
                .map(|call| {
                    size_of::<Call>()
                        + call.id.len()
                        + call.subject.name.len()
                        + call.subject.rest.len()
                        + call.jobs.len() * size_of::<u32>()
                        + call.result.as_ref().map_or(0, |result| result.output.len())
                        + call.activities.as_ref().map_or(0, |activities| {
                            activities
                                .rows
                                .iter()
                                .map(|row| size_of_val(row) + row.title.len())
                                .sum()
                        })
                })
                .sum(),
            _ => 0,
        };
        size_of::<Self>() + self.rows.iter().map(RenderRow::bytes).sum::<usize>() + extra
    }
}

pub(in crate::tui) enum CopySource {
    Text(Arc<Clean>),
    Rows(Arc<Vec<RenderRow>>),
}
