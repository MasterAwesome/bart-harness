use super::*;

impl App {
    pub(in crate::tui) fn invalidate_paint(&mut self) {
        self.paint.record(None);
        self.scrollback.cancel_click();
        self.scrollback.clear_hover();
    }

    pub(super) fn resize_pending(&mut self) {
        self.invalidate_paint();
        self.geometry.resized_at = Some(Instant::now());
    }

    pub(super) fn conversation_painted(&self) -> bool {
        self.paint
            .last
            .as_ref()
            .is_some_and(|frame| frame.conversation && self.frame_size_matches(frame))
    }

    pub(super) fn prepare_frame(&mut self) {
        self.animation_mut().advance();
        if let Some(turn) = &mut self.turn {
            turn.progress.tick();
        }
        self.status.expire();
        if self.drawable() {
            let completed = self.transcript.flush_frame();
            self.append_display(completed);
            if let Some(line) = crate::terminal_text::pending_line() {
                self.set_status(StatusKind::Warning, line);
            }
            if let Some(Surface::Viewer(viewer, _)) = &mut self.surface {
                viewer.prepare(
                    self.geometry.size,
                    self.transcript.details(),
                    &self.session.as_ref().expect("viewer session").live,
                    &mut self.status,
                );
            } else if !matches!(self.surface, Some(Surface::Picker(_))) {
                self.transcript
                    .prepare_tail(self.geometry.size.width, self.animation().tick);
                self.scrollback
                    .prepare(self.geometry.size.width, &self.transcript);
                self.scrollback
                    .prepare_search(&self.transcript, &mut self.status);
            }
        } else {
            self.animation_mut().visible = false;
            if self.geometry.size.width == 0
                || self.geometry.size.height < 3 && !self.confirmation_active()
            {
                self.geometry.resized_at = None;
            }
        }
    }

    pub(super) fn painted(&mut self, painted: Option<view::Painted>) {
        if let Some(painted) = &painted {
            self.animation_mut().visible = painted.spinner_visible;
            self.geometry.resized_at = None;
        } else {
            self.animation_mut().visible = false;
            self.scrollback.cancel_click();
            self.scrollback.clear_hover();
        }
        if let Some(painted) = &painted
            && painted.review
            && let Some(confirm) = self.confirm_mut()
        {
            confirm.painted = Some(painted.stamp);
        }
        if let Some(Surface::Viewer(viewer, _)) = &mut self.surface {
            viewer.painted(painted.as_ref().map(|painted| painted.stamp));
        }
        self.paint.record(painted);
    }

    pub(super) fn resized(&mut self, size: ratatui::layout::Size) {
        if size != self.geometry.size {
            self.geometry.size = size;
            self.scrollback.resize(size.width, &self.transcript);
            self.animation_mut().visible = false;
            self.resize_pending();
        }
    }

    pub(super) fn confirmation_active(&self) -> bool {
        matches!(&self.surface, Some(Surface::Approval(_)))
            || matches!(&self.surface, Some(Surface::Picker(picker)) if picker.confirmation_active())
    }

    fn confirm_mut(&mut self) -> Option<&mut Confirm> {
        match &mut self.surface {
            Some(Surface::Approval(a)) => Some(&mut a.confirm),
            Some(Surface::Picker(picker)) => picker.confirm_mut(),
            _ => None,
        }
    }
    fn frame_size_matches(&self, frame: &view::Painted) -> bool {
        let size = (self.geometry.size.width, self.geometry.size.height);
        self.geometry.resized_at.is_none()
            && frame.stamp.size == size
            && frame
                .terminal_size
                .is_none_or(|probe| probe().ok() == Some(size))
    }

    pub(super) fn review_stamp(&self) -> Option<view::FrameStamp> {
        let frame = self.paint.last.as_ref()?;
        self.frame_size_matches(frame).then_some(frame.stamp)
    }

    pub(in crate::tui) fn drawable(&self) -> bool {
        let size = self.geometry.size;
        size.width > 0
            && size.height > 0
            && (size.height >= 3 || self.confirmation_active())
            && self
                .geometry
                .resized_at
                .is_none_or(|since| since.elapsed() >= Duration::from_millis(75))
    }

    pub(in crate::tui) fn draw(
        &mut self,
        screen: &mut Screen,
        running_jobs: usize,
    ) -> io::Result<view::Painted> {
        let size = self.geometry.size;
        if screen.get_frame().area().as_size() != size {
            screen.resize(ratatui::layout::Rect::from((
                ratatui::layout::Position::ORIGIN,
                size,
            )))?;
        }
        synchronized(screen, |screen| {
            if self.geometry.resized_at.is_some() {
                screen.clear()?;
            }
            let mut painted = self.render_frame(screen, running_jobs)?;
            painted.terminal_size = Some(crossterm::terminal::size);
            Ok(painted)
        })
    }

    pub(in crate::tui) fn deadline(
        &self,
        dirty: bool,
        frame: Instant,
        spin: Instant,
    ) -> Option<Instant> {
        [
            dirty.then_some(frame),
            self.spinning().then_some(spin),
            self.status.until(),
            self.geometry
                .resized_at
                .filter(|_| self.geometry.size.width > 0 && self.geometry.size.height > 0)
                .map(|at| at + Duration::from_millis(75)),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|at| at.max(frame))
    }

    pub(in crate::tui) fn spinning(&self) -> bool {
        self.animation().visible && matches!(self.surface, None | Some(Surface::Approval(_)))
    }

    pub(super) fn animation(&self) -> &Animation {
        self.turn
            .as_ref()
            .map_or(&self.paint.idle_animation, |turn| &turn.animation)
    }

    pub(super) fn animation_mut(&mut self) -> &mut Animation {
        self.turn
            .as_mut()
            .map_or(&mut self.paint.idle_animation, |turn| &mut turn.animation)
    }
}
