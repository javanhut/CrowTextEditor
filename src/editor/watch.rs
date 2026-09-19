//! Keeping buffers honest with the world outside them: files changed on
//! disk, swap files for crash recovery, and change markers from ivaldi.

use super::*;
use std::time::{Duration, Instant};

/// How often open files are stat'ed for changes made elsewhere.
const DISK_CHECK_EVERY: Duration = Duration::from_secs(1);
/// How often unsaved buffers are written to their swap files.
const SWAP_EVERY: Duration = Duration::from_secs(2);
/// How often the sealed text is re-read, in case a seal happened meanwhile.
const VCS_REFRESH_EVERY: Duration = Duration::from_secs(30);
/// Change markers are recomputed once typing pauses this long.
const VCS_DIFF_GAP: Duration = Duration::from_millis(150);

impl Editor {
    /// From the main loop, between keystrokes. True when something on screen
    /// may have changed.
    pub fn watch_tick(&mut self) -> bool {
        let mut changed = false;
        if self.disk_checked.elapsed() >= DISK_CHECK_EVERY {
            self.disk_checked = Instant::now();
            changed |= self.check_disk();
        }
        if self.swap_written.elapsed() >= SWAP_EVERY && self.idle_for(Duration::from_millis(300)) {
            self.swap_written = Instant::now();
            for doc in &mut self.documents {
                doc.write_swap();
            }
        }
        changed |= self.swap_notice();
        changed |= self.vcs_tick();
        changed
    }

    /// Something outside may just have changed files — the terminal window
    /// regained focus, say, or the whole editor did. Check now rather than
    /// on the next timer.
    pub fn external_change_hint(&mut self) {
        let long_ago = Instant::now()
            .checked_sub(VCS_REFRESH_EVERY)
            .unwrap_or_else(Instant::now);
        self.disk_checked = long_ago;
        self.vcs_refreshed = long_ago;
    }

    /// Reload what changed on disk under an unmodified buffer; flag — once —
    /// what changed under a modified one.
    fn check_disk(&mut self) -> bool {
        let mut notes = Vec::new();
        let mut reloaded = Vec::new();
        for (i, doc) in self.documents.iter_mut().enumerate() {
            let Some(path) = doc.path.as_deref() else {
                continue;
            };
            let Some(now) = std::fs::metadata(path).ok().and_then(|m| m.modified().ok()) else {
                continue; // gone, or unreadable: nothing to reload from
            };
            if doc.disk_mtime.is_none() || doc.disk_mtime == Some(now) {
                continue;
            }
            if !doc.modified && crate::config::auto_reload() {
                match doc.reload() {
                    Ok(true) => {
                        notes.push(format!("{} reloaded from disk (u undoes)", doc.name()));
                        reloaded.push(i);
                    }
                    Ok(false) => {}
                    // Unreadable now (not UTF-8, replaced by a directory):
                    // say so once instead of retrying every second in silence.
                    Err(e) => {
                        if doc.disk_conflict != Some(now) {
                            doc.disk_conflict = Some(now);
                            notes.push(format!("{}: {e}", doc.name()));
                        }
                    }
                }
            } else if doc.disk_conflict != Some(now) {
                doc.disk_conflict = Some(now);
                notes.push(format!(
                    "{} changed on disk — :e! loads it, :w! keeps yours",
                    doc.name()
                ));
            }
        }
        for i in reloaded {
            self.request_vcs(i);
        }
        if notes.is_empty() {
            return false;
        }
        self.set_status(notes.join("; "));
        true
    }

    /// Mention a swap file with unsaved work in it, once, when its buffer is
    /// the one in front of you.
    fn swap_notice(&mut self) -> bool {
        let doc = self.doc_mut();
        if !doc.swap_found || doc.swap_notified {
            return false;
        }
        doc.swap_notified = true;
        let name = doc.name();
        self.set_status(format!(
            "{name}: unsaved changes from an earlier session — :recover restores them, :recover! discards"
        ));
        true
    }

    /// `:recover` — load the swap file's text into the buffer, as an undoable
    /// edit for you to look over and write.
    pub(crate) fn recover_swap(&mut self) {
        let Some(text) = self.doc().read_swap() else {
            self.set_status("no swap file for this buffer");
            return;
        };
        let doc = self.doc_mut();
        let changed = doc.replace_all(&text);
        doc.swap_found = false;
        self.set_status(if changed {
            "recovered the unsaved changes — :w keeps them, u takes them back"
        } else {
            "the swap file matches the buffer"
        });
    }

    /// `:wa` — write every modified buffer that has a file.
    pub(crate) fn write_all(&mut self) {
        let current = self.current;
        let mut written = 0;
        let mut failed = Vec::new();
        for i in 0..self.documents.len() {
            let doc = &self.documents[i];
            if !doc.modified || doc.path.is_none() {
                continue;
            }
            self.current = i;
            commands::save_with(self, false);
            if self.documents[i].modified {
                failed.push(self.documents[i].name());
            } else {
                written += 1;
            }
        }
        self.current = current;
        self.set_status(match (written, failed.is_empty()) {
            (0, true) => "nothing to write".to_string(),
            (n, true) => format!("{n} buffer{} written", if n == 1 { "" } else { "s" }),
            (_, false) => format!("not written: {} (:w! each to overwrite)", failed.join(", ")),
        });
    }

    /// Leaving the editor: swap files of buffers with nothing unsaved go,
    /// and so do all of them after `:q!`, which asked for exactly that.
    /// Unsaved work anywhere else keeps its swap file for next time — and so
    /// does work from an earlier session that was noticed but never
    /// recovered, which quitting must not be a way to throw away.
    pub fn cleanup_on_exit(&mut self) {
        let discard = self.quit_discarding;
        for doc in &mut self.documents {
            if discard || (!doc.modified && !doc.swap_found) {
                doc.remove_swap();
            } else {
                doc.write_swap();
            }
        }
    }

    // ---- ivaldi change markers ----------------------------------------------

    /// Ask ivaldi, in the background, for buffer `i`'s sealed text.
    pub(crate) fn request_vcs(&mut self, i: usize) {
        if !crate::config::vcs_gutter() {
            return;
        }
        let Some(path) = self.documents[i].path.clone() else {
            return;
        };
        let Ok(canon) = path.canonicalize() else {
            // Not on disk yet; saving it asks again.
            self.documents[i].vcs.base = crate::vcs::Base::NoRepo;
            return;
        };
        self.documents[i].vcs.pending = true;
        let tx = self.vcs_tx.clone();
        std::thread::spawn(move || {
            let base = crate::vcs::fetch_base(&canon);
            let _ = tx.send((i, canon, base));
        });
    }

    fn vcs_tick(&mut self) -> bool {
        if !crate::config::vcs_gutter() {
            return false;
        }
        let mut changed = false;
        while let Ok((i, path, base)) = self.vcs_rx.try_recv() {
            let Some(doc) = self.documents.get_mut(i) else {
                continue;
            };
            // Always clear the flag, even when the answer is about a file the
            // buffer no longer points at (renamed, deleted, saved elsewhere
            // while we asked) — leaving it set would stop it ever asking again.
            doc.vcs.pending = false;
            let still_there = doc
                .path
                .as_ref()
                .is_some_and(|p| p.canonicalize().is_ok_and(|p| p == path));
            if still_there {
                doc.vcs.base = base;
                doc.vcs.marks_revision = None;
                changed = true;
            }
        }
        let refresh = self.vcs_refreshed.elapsed() >= VCS_REFRESH_EVERY;
        if refresh {
            self.vcs_refreshed = Instant::now();
        }
        // Every buffer is asked about once; the periodic re-ask is only for
        // the ones on screen, so a session with thirty buffers open doesn't
        // run thirty `ivaldi` processes every half minute.
        let visible = self.visible_docs();
        for i in 0..self.documents.len() {
            let doc = &self.documents[i];
            let unknown = doc.vcs.base == crate::vcs::Base::Unknown;
            if doc.path.is_some()
                && !doc.vcs.pending
                && (unknown || (refresh && visible.contains(&i)))
            {
                self.request_vcs(i);
            }
        }
        // Only what is on screen: the marks of a buffer nobody is looking at
        // are recomputed when it comes back into view.
        if self.idle_for(VCS_DIFF_GAP) {
            for i in visible {
                let doc = &mut self.documents[i];
                if doc.vcs.base == crate::vcs::Base::Unknown
                    || doc.vcs.marks_revision == Some(doc.revision)
                {
                    continue;
                }
                let lines: Vec<u64> = (0..doc.line_count())
                    .map(|l| crate::vcs::hash_line(doc.line(l)))
                    .collect();
                let revision = doc.revision;
                doc.vcs.recompute(lines, revision);
                changed = true;
            }
        }
        changed
    }

    /// The buffers a window is showing, the focused one included.
    fn visible_docs(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        self.layout.leaf_ids(&mut ids);
        let mut docs: Vec<usize> = ids
            .into_iter()
            .filter_map(|id| self.layout.find(id).map(|w| w.doc))
            .chain(std::iter::once(self.current))
            .collect();
        docs.sort_unstable();
        docs.dedup();
        docs
    }

    /// `]g` / `[g`: the next (or previous) changed stretch, wrapping around.
    pub fn goto_change(&mut self, forward: bool) {
        let starts = self.doc().vcs.hunk_starts();
        if starts.is_empty() {
            self.set_status("no changes since the last seal");
            return;
        }
        let line = self.doc().cursor_line();
        let target = if forward {
            starts.iter().find(|&&l| l > line).or(starts.first())
        } else {
            starts.iter().rev().find(|&&l| l < line).or(starts.last())
        };
        let Some(&target) = target else {
            return;
        };
        self.push_jump();
        let doc = self.doc_mut();
        doc.cursor = doc.line_start(target.min(doc.line_count().saturating_sub(1)));
        doc.anchor = doc.cursor;
        doc.goal_col = None;
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::tests::{editor_with, press};

    #[test]
    fn an_unmodified_buffer_follows_its_file_and_a_modified_one_is_flagged() {
        let dir = std::env::temp_dir().join(format!("crow-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("w.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let mut editor = editor_with("");
        editor.jump_to(file.clone(), 1, 0);
        std::fs::write(&file, "one\nTWO\nthree\n").unwrap();
        // Pretend the stamp is older than the write.
        editor.doc_mut().disk_mtime = Some(std::time::SystemTime::UNIX_EPOCH);
        assert!(editor.check_disk());
        assert_eq!(editor.doc().text.to_string(), "one\nTWO\nthree\n");
        assert!(!editor.doc().modified);
        assert_eq!(
            editor.doc().cursor_line(),
            1,
            "the cursor stays on its line"
        );
        press(&mut editor, "u");
        assert_eq!(
            editor.doc().text.to_string(),
            "one\ntwo\n",
            "a reload is undoable"
        );

        // Now with edits of our own: no reload, one warning.
        press(&mut editor, "A ! <esc>");
        std::fs::write(&file, "theirs\n").unwrap();
        editor.doc_mut().disk_mtime = Some(std::time::SystemTime::UNIX_EPOCH);
        assert!(editor.check_disk());
        assert!(editor.status.contains("changed on disk"));
        assert!(editor.doc().text.to_string().contains('!'));
        assert!(!editor.check_disk(), "flagged once, not every second");
        press(&mut editor, ": e! <enter>");
        assert_eq!(editor.doc().text.to_string(), "theirs\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_files_are_written_found_and_recovered() {
        let dir = std::env::temp_dir().join(format!("crow-swap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.txt");
        std::fs::write(&file, "saved\n").unwrap();
        {
            let mut editor = editor_with("");
            editor.jump_to(file.clone(), 0, 0);
            press(&mut editor, "A <space> unsaved <esc>");
            assert!(editor.doc_mut().write_swap());
            // A crash: no cleanup, the swap file stays.
        }
        let mut editor = editor_with("");
        editor.jump_to(file.clone(), 0, 0);
        assert!(editor.doc().swap_found);
        assert!(editor.watch_tick() || editor.status.contains(":recover"));
        press(&mut editor, ": recover <enter>");
        assert_eq!(editor.doc().text.to_string(), "saved unsaved\n");
        assert!(editor.doc().modified);
        press(&mut editor, ": w <enter>");
        assert!(
            editor.doc().read_swap().is_none(),
            "writing clears the swap file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The recovery window has to survive both of the things that happen
    /// next: typing, and quitting.
    #[test]
    fn an_unrecovered_swap_file_survives_editing_and_quitting() {
        let dir = std::env::temp_dir().join(format!("crow-swapkeep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("k.txt");
        std::fs::write(&file, "saved\n").unwrap();
        {
            let mut editor = editor_with("");
            editor.jump_to(file.clone(), 0, 0);
            press(&mut editor, "A <space> precious <esc>");
            assert!(editor.doc_mut().write_swap());
        } // a crash: no cleanup

        let mut editor = editor_with("");
        editor.jump_to(file.clone(), 0, 0);
        assert!(editor.doc().swap_found);
        // Typing before recovering must not overwrite what is there.
        press(&mut editor, "A x <esc>");
        assert!(!editor.doc_mut().write_swap());
        editor.watch_tick();
        assert_eq!(
            editor.doc().read_swap().as_deref(),
            Some("saved precious\n"),
            "the earlier session's text is still there to recover"
        );
        // Nor may quitting take it, even with nothing of our own unsaved.
        press(&mut editor, ": w <enter>");
        editor.cleanup_on_exit();
        assert!(editor.doc().read_swap().is_some(), "quitting kept it");

        // `:recover` and `:recover!` are the two ways it goes away.
        editor.recover_swap();
        assert!(!editor.doc().swap_found);
        assert!(
            editor.doc_mut().write_swap(),
            "now our own text is saved aside"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `:w other.txt` leaves the file it came from; its swap file must not
    /// stay behind offering that text for the old file.
    #[test]
    fn saving_under_a_new_name_takes_the_old_swap_file_with_it() {
        let dir = std::env::temp_dir().join(format!("crow-swapmove-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("old.txt"), dir.join("new.txt"));
        std::fs::write(&old, "old\n").unwrap();
        let mut editor = editor_with("");
        editor.jump_to(old.clone(), 0, 0);
        press(&mut editor, "I X <esc>");
        assert!(editor.doc_mut().write_swap());
        editor.doc_mut().save_as(&new, false).unwrap();
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "Xold\n");

        let reopened = crate::document::Document::open(&old).unwrap();
        assert!(!reopened.swap_found, "no stale swap for the file we left");
        assert!(reopened.read_swap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_all_writes_every_modified_buffer() {
        let dir = std::env::temp_dir().join(format!("crow-wa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "a\n").unwrap();
        std::fs::write(&b, "b\n").unwrap();
        let mut editor = editor_with("");
        editor.jump_to(a.clone(), 0, 0);
        press(&mut editor, "A 1 <esc>");
        editor.jump_to(b.clone(), 0, 0);
        press(&mut editor, "A 2 <esc>");
        press(&mut editor, ": wa <enter>");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a1\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b2\n");
        assert!(editor.status.contains("2 buffers written"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
