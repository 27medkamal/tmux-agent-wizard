//! The directories the `/` filter lists under the tree (`[ui] projects_in_filter`).
//!
//! What is listed is the project picker's own list for what is typed in the filter, cut
//! to [`FILTER_PROJECTS`] ([`picker::projects`]); this file is when it is listed again.
//! The list is made, reading the directories for path completion and the project
//! directories' children, for a keystroke that changes the text, for the filter opening, and
//! once zoxide has answered. The popup's two-a-second tick makes no list: it tells the tags
//! again ([`picker::retag`]), which still asks the filesystem for the canonical path of each
//! session's directory and of each listed one, to know where a session is running.

use super::{App, FILTER_PROJECTS};
use crate::tui::picker;

impl App<'_> {
    /// Whether the filter lists directories right now: `[ui] projects_in_filter` is on and
    /// the filter is open, the modal preset's after `/` until Esc, the search preset's
    /// always.
    pub fn projects_shown(&self) -> bool {
        self.cfg.ui.projects_in_filter && self.filter.is_some()
    }

    /// zoxide's directories, once the run loop has asked zoxide: for the picker, and for
    /// the filter's list. The search preset's filter is open from the first frame, and this
    /// is where its directories are first listed, with zoxide's in them, so a popup that has
    /// not been handed zoxide's answer lists none until a key is typed.
    pub fn set_zoxide(&mut self, dirs: Vec<String>) {
        self.zoxide = dirs;
        if self.projects_shown() {
            let keep = self.row_id();
            self.refresh_projects();
            self.rebuild();
            self.restore_cursor(keep);
        }
    }

    /// Opens the new-session picker over the tree without a key, for a popup started on it
    /// (`taw ui --new-session`, which `taw open` with nothing to open runs). After
    /// [`App::set_zoxide`], so the picker lists zoxide's directories from its first frame.
    ///
    /// Esc on this picker closes the popup, where Esc on the one `n` opens goes back to the
    /// tree: the popup was opened to pick a directory, and Esc says none was wanted.
    pub fn start_on_new_session_picker(&mut self) {
        self.open_project_picker();
        self.started_on_picker = true;
    }

    /// Lists the directories again for what is typed in the filter, or none when they are
    /// not shown. The caller rebuilds the rows. A new list starts at its top, and the section
    /// may take more or fewer of the panel's lines than the last, so the cursor is looked
    /// for on screen again.
    pub(super) fn refresh_projects(&mut self) {
        self.projects_scroll = 0;
        self.follow_sel = true;
        self.projects = if self.projects_shown() {
            let typed = self
                .filter
                .as_ref()
                .map(|input| input.value().trim().to_string())
                .unwrap_or_default();
            picker::projects(
                &typed,
                &self.snap,
                self.cfg,
                &self.home,
                &self.zoxide,
                FILTER_PROJECTS,
            )
        } else {
            Vec::new()
        };
    }

    /// Tells each listed directory's tag again from the snapshot, without listing anything:
    /// a session that opened where one is listed makes it `running`, one that closed makes it
    /// what it was. The rows are the same ones, so nothing is rebuilt here.
    pub(super) fn retag_projects(&mut self) {
        if self.projects.is_empty() {
            return;
        }
        picker::retag(
            &mut self.projects,
            &self.snap,
            self.cfg,
            &self.home,
            &self.zoxide,
        );
    }
}

#[cfg(test)]
mod tests;
