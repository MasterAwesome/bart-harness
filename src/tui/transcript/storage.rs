use super::*;

impl Transcript {
    pub(in crate::tui) fn append(
        &mut self,
        blocks: impl IntoIterator<Item = Block>,
        protected: Option<(u64, u64)>,
    ) -> Vec<u64> {
        let mut removed = Vec::new();
        for block in blocks {
            self.push(block);
        }
        while self.bytes > MAX_BLOCK_BYTES || self.rows > MAX_ROWS {
            let index = self.blocks.iter().position(|entry| {
                protected.is_none_or(|(start, end)| entry.id < start || entry.id > end)
            });
            let Some(index) = index else { break };
            let entry = self.blocks.remove(index).expect("retained index");
            self.bytes -= entry.block.bytes();
            self.rows -= entry.block.rows.len();
            removed.push(entry.id);
        }
        removed
    }
    fn push(&mut self, mut block: Block) {
        for row in Arc::make_mut(&mut block.rows) {
            row.bound_table(row.bytes());
        }
        self.bytes += block.bytes();
        self.rows += block.rows.len();
        self.blocks.push_back(StoredBlock {
            id: self.next,
            block,
        });
        self.next += 1;
    }
    pub(in crate::tui) fn blocks(&self) -> &std::collections::VecDeque<StoredBlock> {
        &self.blocks
    }
    pub(in crate::tui) fn first(&self) -> u64 {
        self.blocks.front().map_or(self.next, |entry| entry.id)
    }
    pub(in crate::tui) fn next(&self) -> u64 {
        self.next
    }
    pub(in crate::tui) fn ui(&self) -> crate::preferences::Ui {
        self.ui
    }
    pub(in crate::tui) fn revision(&self) -> u64 {
        self.revision
    }
    pub(in crate::tui) fn tail(&self) -> &[RenderRow] {
        &self.tail
    }
    pub(in crate::tui) fn copy_sources(&self) -> Vec<CopySource> {
        self.blocks
            .iter()
            .map(|entry| entry.block.source())
            .collect()
    }
}
