//! Edits are modelled as *changesets* over the whole document rather than as
//! direct mutations of the rope.
//!
//! A `Transaction` is a sequence of operations that, read left to right,
//! describes the entire document: retain N characters, delete N characters,
//! insert this text. Because a transaction describes the whole document, it can
//! be inverted against the original text to produce an exact undo, and two
//! transactions over the same document can be composed or rebased against each
//! other.
//!
//! This is the single most important design decision in the editor. Multiple
//! cursors, macros, and collaborative editing all reduce to operations on
//! changesets. Mutating the rope directly and adding undo later means rewriting
//! the core.

use ropey::Rope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Advance the cursor N characters, leaving them unchanged.
    Retain(usize),
    /// Delete the next N characters.
    Delete(usize),
    /// Insert this text at the current position.
    Insert(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transaction {
    ops: Vec<Operation>,
}

impl Transaction {
    pub fn new() -> Self {
        Transaction { ops: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.ops
            .iter()
            .all(|op| matches!(op, Operation::Retain(_)))
    }

    pub fn ops(&self) -> &[Operation] {
        &self.ops
    }

    /// Build a transaction from a set of changes over `text`.
    ///
    /// Each change is `(from_char, to_char, replacement)`. Deleting is
    /// `(from, to, None)`, inserting is `(at, at, Some(s))`, replacing is
    /// `(from, to, Some(s))`.
    ///
    /// Changes must be sorted by `from` and must not overlap. This is checked
    /// with a debug assertion rather than at runtime.
    pub fn change<I>(text: &Rope, changes: I) -> Self
    where
        I: IntoIterator<Item = (usize, usize, Option<String>)>,
    {
        let mut ops: Vec<Operation> = Vec::new();
        let mut last = 0usize;

        for (from, to, replacement) in changes {
            debug_assert!(from >= last, "changes must be sorted and non-overlapping");
            debug_assert!(to >= from, "change range must be well-formed");

            if from > last {
                ops.push(Operation::Retain(from - last));
            }
            if let Some(s) = replacement {
                if !s.is_empty() {
                    ops.push(Operation::Insert(s));
                }
            }
            if to > from {
                ops.push(Operation::Delete(to - from));
            }
            last = to;
        }

        let len = text.len_chars();
        debug_assert!(last <= len, "change extends past end of document");
        if last < len {
            ops.push(Operation::Retain(len - last));
        }

        Transaction { ops }
    }

    /// Convenience constructor: insert `s` at char index `at`.
    pub fn insert(text: &Rope, at: usize, s: impl Into<String>) -> Self {
        Self::change(text, [(at, at, Some(s.into()))])
    }

    /// Convenience constructor: delete the char range `from..to`.
    pub fn delete(text: &Rope, from: usize, to: usize) -> Self {
        Self::change(text, [(from, to, None)])
    }

    /// Apply this transaction to `text` in place.
    ///
    /// Processing left to right keeps `pos` valid in the *partially rewritten*
    /// document: retained and inserted text is behind the cursor and already
    /// final, so no offset adjustment is needed.
    pub fn apply(&self, text: &mut Rope) {
        let mut pos = 0usize;
        for op in &self.ops {
            match op {
                Operation::Retain(n) => pos += n,
                Operation::Delete(n) => {
                    text.remove(pos..pos + n);
                }
                Operation::Insert(s) => {
                    text.insert(pos, s);
                    pos += s.chars().count();
                }
            }
        }
    }

    /// Produce the transaction that undoes this one.
    ///
    /// `original` must be the document *before* this transaction was applied,
    /// since the deleted text has to be recovered from it.
    pub fn invert(&self, original: &Rope) -> Self {
        let mut ops = Vec::with_capacity(self.ops.len());
        let mut pos = 0usize;

        for op in &self.ops {
            match op {
                Operation::Retain(n) => {
                    ops.push(Operation::Retain(*n));
                    pos += n;
                }
                Operation::Delete(n) => {
                    let deleted = original.slice(pos..pos + n).to_string();
                    ops.push(Operation::Insert(deleted));
                    pos += n;
                }
                Operation::Insert(s) => {
                    ops.push(Operation::Delete(s.chars().count()));
                }
            }
        }

        Transaction { ops }
    }

    /// Map a position through this transaction, so a cursor or mark survives an
    /// edit made elsewhere in the document.
    ///
    /// `assoc_before` decides which side of an insertion at exactly `pos` the
    /// position lands on.
    pub fn map_pos(&self, pos: usize, assoc_before: bool) -> usize {
        let mut old = 0usize;
        let mut new = 0usize;

        for op in &self.ops {
            match op {
                Operation::Retain(n) => {
                    if old + n > pos {
                        return new + (pos - old);
                    }
                    old += n;
                    new += n;
                }
                Operation::Delete(n) => {
                    if old + n > pos {
                        // Position was inside deleted text; collapse to the start.
                        return new;
                    }
                    old += n;
                }
                Operation::Insert(s) => {
                    let len = s.chars().count();
                    if old == pos && assoc_before {
                        return new;
                    }
                    new += len;
                }
            }
        }

        new + pos.saturating_sub(old)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_invert_roundtrips() {
        let original = Rope::from_str("hello world");
        let mut text = original.clone();

        let tx = Transaction::insert(&text, 5, ",");
        tx.apply(&mut text);
        assert_eq!(text.to_string(), "hello, world");

        tx.invert(&original).apply(&mut text);
        assert_eq!(text.to_string(), "hello world");
    }

    #[test]
    fn delete_then_invert_roundtrips() {
        let original = Rope::from_str("hello world");
        let mut text = original.clone();

        let tx = Transaction::delete(&text, 0, 6);
        tx.apply(&mut text);
        assert_eq!(text.to_string(), "world");

        tx.invert(&original).apply(&mut text);
        assert_eq!(text.to_string(), "hello world");
    }

    #[test]
    fn multiple_changes_apply_left_to_right() {
        let original = Rope::from_str("aaa bbb ccc");
        let mut text = original.clone();

        let tx = Transaction::change(
            &text,
            [
                (0, 3, Some("xxx".to_string())),
                (8, 11, Some("zzz".to_string())),
            ],
        );
        tx.apply(&mut text);
        assert_eq!(text.to_string(), "xxx bbb zzz");

        tx.invert(&original).apply(&mut text);
        assert_eq!(text.to_string(), "aaa bbb ccc");
    }

    #[test]
    fn positions_shift_through_edits() {
        let text = Rope::from_str("hello world");
        let tx = Transaction::insert(&text, 0, "-> ");
        // A cursor at "world" moves right by the length of the insertion.
        assert_eq!(tx.map_pos(6, false), 9);
    }

    #[test]
    fn position_inside_deletion_collapses() {
        let text = Rope::from_str("hello world");
        let tx = Transaction::delete(&text, 0, 6);
        assert_eq!(tx.map_pos(3, false), 0);
        assert_eq!(tx.map_pos(8, false), 2);
    }
}
