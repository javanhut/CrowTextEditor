//! Project-wide replace: the grep picker's hits become the lines of one
//! scratch buffer, `path:line: text`, edited with everything crow edits
//! with — `s`, multi-cursor, `:%s` — and `:w` there writes each changed line
//! back into its file.

use super::*;

/// One line of a refactor buffer: the file and line it came from, and the
/// text it had there, which is what a write checks it is still replacing.
#[derive(Debug, Clone)]
pub struct RefactorLine {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

impl RefactorLine {
    /// What the buffer line starts with, and must still start with.
    fn prefix(&self, root: &Path) -> String {
        let rel = self.path.strip_prefix(root).unwrap_or(&self.path);
        format!("{}:{}: ", rel.display(), self.line + 1)
    }
}

impl Editor {
    /// `C-e` in the grep picker: every hit so far, in a buffer to edit.
    pub(crate) fn grep_to_refactor(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let crate::picker::Kind::Grep { root, .. } = &picker.kind else {
            return;
        };
        let root = root.clone();
        let hits: Vec<(PathBuf, usize)> = picker
            .items
            .iter()
            .filter_map(|item| {
                let (path, line) = item.label.rsplit_once(':')?;
                Some((root.join(path), line.parse::<usize>().ok()?.checked_sub(1)?))
            })
            .collect();
        self.close_picker();
        if hits.is_empty() {
            self.set_status("no matches to edit");
            return;
        }
        // The full line, not the picker's trimmed and clipped copy of it —
        // from the buffer when the file is open, since that is what a write
        // will edit.
        let mut files: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let lines: Vec<RefactorLine> =
            hits.into_iter()
                .filter_map(|(path, line)| {
                    let file = files.entry(path.clone()).or_insert_with(|| {
                        match self.open_buffer(&path) {
                            Some(i) => {
                                let doc = &self.documents[i];
                                (0..doc.line_count())
                                    .map(|l| doc.line(l).chars().take(doc.line_len(l)).collect())
                                    .collect()
                            }
                            None => std::fs::read_to_string(&path)
                                .unwrap_or_default()
                                .lines()
                                .map(str::to_string)
                                .collect(),
                        }
                    });
                    Some(RefactorLine {
                        text: file.get(line)?.clone(),
                        path,
                        line,
                    })
                })
                .collect();
        let text: String = lines
            .iter()
            .map(|l| format!("{}{}\n", l.prefix(&root), l.text))
            .collect();
        let mut doc = Document::empty();
        doc.text = ropey::Rope::from_str(&text);
        doc.refactor = Some((root, lines));
        self.leave_terminal_for_edit();
        self.documents.push(doc);
        self.current = self.documents.len() - 1;
        self.set_status("edit the lines, then :w writes them back to their files");
    }

    /// `:w` in a refactor buffer. Lines that changed go back to their files
    /// as one undo step per file; a file that was unmodified is saved, one
    /// with unsaved work of its own is left for you to `:w`. A line whose
    /// file no longer says what it said is skipped, not clobbered.
    pub(crate) fn write_refactor(&mut self) {
        let Some((root, entries)) = self.doc().refactor.clone() else {
            return;
        };
        let text = self.doc().text.to_string();
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() != entries.len() {
            self.set_status(format!(
                "keep one line per match: {} lines for {} matches",
                lines.len(),
                entries.len()
            ));
            return;
        }
        // Per file: (buffer line, file line, new text) for each change.
        type Changes = Vec<(usize, usize, String)>;
        let mut per_file: Vec<(PathBuf, Changes)> = Vec::new();
        for (i, (entry, line)) in entries.iter().zip(&lines).enumerate() {
            let Some(new) = line.strip_prefix(&entry.prefix(&root)) else {
                self.set_status(format!(
                    "line {}: its `path:line:` prefix was edited",
                    i + 1
                ));
                return;
            };
            if new == entry.text {
                continue;
            }
            let item = (i, entry.line, new.to_string());
            match per_file.iter_mut().find(|(p, _)| *p == entry.path) {
                Some((_, v)) => v.push(item),
                None => per_file.push((entry.path.clone(), vec![item])),
            }
        }

        let me = self.current;
        let mut entries = entries;
        let (mut changed, mut stale, mut unsaved) = (0, 0, Vec::new());
        for (path, edits) in per_file {
            let Some(idx) = self.buffer_for(&path) else {
                stale += edits.len();
                continue;
            };
            let doc = &mut self.documents[idx];
            let was_modified = doc.modified;
            let mut changes = Vec::new();
            for (i, line, new) in edits {
                let current: Option<String> = (line < doc.line_count())
                    .then(|| doc.line(line).chars().take(doc.line_len(line)).collect());
                if current.as_deref() != Some(entries[i].text.as_str()) {
                    stale += 1;
                    continue;
                }
                let start = doc.line_start(line);
                changes.push((start, start + doc.line_len(line), Some(new.clone())));
                entries[i].text = new;
            }
            if changes.is_empty() {
                continue;
            }
            changed += changes.len();
            changes.sort_by_key(|c| c.0);
            let tx = crate::transaction::Transaction::change(&doc.text, changes);
            let cursor = tx.map_pos(doc.cursor, false);
            doc.commit_undo_group();
            doc.apply(tx, cursor);
            doc.anchor = doc.cursor;
            doc.commit_undo_group();
            if was_modified {
                unsaved.push(doc.name());
            } else if let Err(e) = doc.save(false) {
                unsaved.push(format!("{} ({e})", doc.name()));
            }
        }
        let doc = &mut self.documents[me];
        doc.refactor = Some((root, entries));
        doc.modified = false;
        let mut msg = format!(
            "{changed} line{} written back",
            if changed == 1 { "" } else { "s" }
        );
        if stale > 0 {
            msg.push_str(&format!(", {stale} skipped (their file changed)"));
        }
        if !unsaved.is_empty() {
            msg.push_str(&format!(", unsaved: {}", unsaved.join(", ")));
        }
        self.set_status(msg);
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::tests::{editor_with, press};

    #[test]
    fn grep_hits_edit_as_lines_and_write_back() {
        let dir = std::env::temp_dir().join(format!("crow-refactor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "  old one\nkeep\n").unwrap();
        std::fs::write(&b, "old two\n").unwrap();

        let mut editor = editor_with("");
        let mut picker = crate::picker::Picker::grep(&dir, None);
        picker.items = vec![
            crate::picker::Item {
                label: "a.txt:1".into(),
                detail: "old one".into(),
            },
            crate::picker::Item {
                label: "b.txt:1".into(),
                detail: "old two".into(),
            },
        ];
        editor.open_picker(picker);
        press(&mut editor, "C-e");
        assert_eq!(
            editor.doc().text.to_string(),
            "a.txt:1:   old one\nb.txt:1: old two\n"
        );

        press(&mut editor, ": %s/old/new/g <enter> : w <enter>");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "  new one\nkeep\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "new two\n");
        assert!(!editor.doc().modified);

        // A mangled prefix is refused rather than guessed at.
        press(&mut editor, "g g i Z <esc> : w <enter>");
        assert!(editor.status.contains("prefix"), "{}", editor.status);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
