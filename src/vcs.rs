//! Change markers against the last ivaldi seal.
//!
//! Ivaldi has no "print this file at HEAD" command, but `ivaldi whodidit`
//! prints the sealed text line by line (with blame in front), so that is the
//! base. It runs in a background thread — a subprocess per file is not
//! something to do between keystrokes — and the diff against the buffer is
//! done here, in memory, whenever the buffer has changed and typing pauses.

use std::path::{Path, PathBuf};

/// What one buffer line looks like against the sealed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    None,
    Added,
    Modified,
    /// Sealed lines were removed just above this line.
    DeletedAbove,
}

/// The sealed version of a file, as far as we know it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Base {
    /// Not asked yet.
    #[default]
    Unknown,
    /// Not in an ivaldi repository (or no `ivaldi` on PATH): no markers.
    NoRepo,
    /// In a repository but never sealed: every line is new.
    Untracked,
    /// The sealed lines, hashed.
    Tracked(Vec<u64>),
}

#[derive(Default)]
pub struct DocState {
    pub base: Base,
    /// A `whodidit` is in flight for this buffer.
    pub pending: bool,
    /// One mark per buffer line, computed for `marks_revision`.
    pub marks: Vec<Mark>,
    pub marks_revision: Option<u64>,
    /// (added, modified, deleted) line counts, for the status line.
    pub stats: (usize, usize, usize),
}

impl DocState {
    /// The mark for `line`, `Mark::None` when there is nothing to show.
    pub fn mark(&self, line: usize) -> Mark {
        self.marks.get(line).copied().unwrap_or(Mark::None)
    }

    /// First line of every changed stretch, top to bottom.
    pub fn hunk_starts(&self) -> Vec<usize> {
        (0..self.marks.len())
            .filter(|&i| self.marks[i] != Mark::None && (i == 0 || self.marks[i - 1] == Mark::None))
            .collect()
    }

    /// Recompute the marks from the buffer's current line hashes (see
    /// `hash_line`), which the caller reads straight off the rope.
    pub fn recompute(&mut self, current: Vec<u64>, revision: u64) {
        self.marks = match &self.base {
            Base::Unknown | Base::NoRepo => Vec::new(),
            Base::Untracked => vec![Mark::Added; current.len()],
            Base::Tracked(base) => marks(base, &current),
        };
        self.stats = self
            .marks
            .iter()
            .fold((0, 0, 0), |(a, m, d), mark| match mark {
                Mark::Added => (a + 1, m, d),
                Mark::Modified => (a, m + 1, d),
                Mark::DeletedAbove => (a, m, d + 1),
                Mark::None => (a, m, d),
            });
        self.marks_revision = Some(revision);
    }
}

/// The ivaldi repository `path` lives in: the nearest ancestor with a
/// `.ivaldi` directory.
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let abs = path.canonicalize().ok()?;
    abs.ancestors()
        .skip(1)
        .find(|dir| dir.join(".ivaldi").is_dir())
        .map(Path::to_path_buf)
}

/// Ask ivaldi for the sealed text of `path`. Blocking — call it from a thread.
pub fn fetch_base(path: &Path) -> Base {
    let Some(root) = repo_root(path) else {
        return Base::NoRepo;
    };
    let Ok(abs) = path.canonicalize() else {
        return Base::NoRepo;
    };
    let Ok(rel) = abs.strip_prefix(&root) else {
        return Base::NoRepo;
    };
    let output = std::process::Command::new("ivaldi")
        .arg("whodidit")
        .arg(rel)
        .current_dir(&root)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return Base::NoRepo; // ivaldi isn't installed
    };
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return if err.contains("not found at HEAD") {
            Base::Untracked
        } else {
            Base::NoRepo
        };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Base::Tracked(
        parse_whodidit(&text)
            .iter()
            .map(|l| hash(l.as_bytes()))
            .collect(),
    )
}

/// The file's lines out of `whodidit` output. Every line carries its line
/// number as `N)  ` followed by the text; the first line of each seal's run
/// has the seal and author in front, the rest are indented to match.
pub fn parse_whodidit(output: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for row in output.lines() {
        let Some(at) = number_marker(row) else {
            continue;
        };
        lines.push(row[at..].to_string());
    }
    lines
}

/// Byte offset just past the first `<digits>)  ` in `row` that starts a word.
fn number_marker(row: &str) -> Option<usize> {
    let bytes = row.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let word_start = i == 0 || bytes[i - 1] == b' ';
        if word_start && bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if row[j..].starts_with(")  ") {
                return Some(j + 3);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// One line of a rope, hashed without building a `String` for it: the diff
/// only ever compares lines for equality, and this runs over the whole file
/// every time typing pauses.
pub fn hash_line(line: ropey::RopeSlice) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut buf = [0u8; 4];
    for c in line.chars() {
        if c == '\n' || c == '\r' {
            break;
        }
        for &b in c.encode_utf8(&mut buf).as_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// FNV-1a: equal lines hash equal, which is all the diff needs.
fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Equal,
    Delete,
    Insert,
}

/// Past this many edits the diff stops looking for the shortest script and
/// marks the whole differing middle as changed.
///
/// Myers costs O(D²) in time and in the trace it keeps to walk back through,
/// and this runs after every pause in typing: at 4000 it was a tenth of a
/// second and 128 MB, which is a visible stall for an answer nobody reads
/// line by line. At 400 it is under a millisecond, and a file that differs by
/// more than a few hundred lines is one you already know you rewrote.
const MAX_EDITS: usize = 400;

/// One mark per line of `current`.
fn marks(base: &[u64], current: &[u64]) -> Vec<Mark> {
    let mut out = vec![Mark::None; current.len()];
    // Trim what both ends share: most edits touch a few lines of a big file.
    let mut pre = 0;
    while pre < base.len() && pre < current.len() && base[pre] == current[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < base.len() - pre
        && suf < current.len() - pre
        && base[base.len() - 1 - suf] == current[current.len() - 1 - suf]
    {
        suf += 1;
    }
    let a = &base[pre..base.len() - suf];
    let b = &current[pre..current.len() - suf];
    let script = myers(a, b).unwrap_or_else(|| {
        // Too different to be worth aligning: call it all modified.
        let mut s = vec![Edit::Delete; a.len()];
        s.extend(std::iter::repeat_n(Edit::Insert, b.len()));
        s
    });

    // Walk the script in hunks: a run of deletes and inserts between equal
    // lines. Paired lines are modified, extra inserts added, and extra
    // deletes leave a marker on the line below them.
    let mut line = pre; // index into `current`
    let mut i = 0;
    while i < script.len() {
        if script[i] == Edit::Equal {
            line += 1;
            i += 1;
            continue;
        }
        let (mut dels, mut ins) = (0, 0);
        while i < script.len() && script[i] != Edit::Equal {
            match script[i] {
                Edit::Delete => dels += 1,
                _ => ins += 1,
            }
            i += 1;
        }
        for k in 0..ins {
            out[line + k] = if k < dels {
                Mark::Modified
            } else {
                Mark::Added
            };
        }
        if ins == 0 && !out.is_empty() {
            let at = line.min(out.len() - 1);
            if out[at] == Mark::None {
                out[at] = Mark::DeletedAbove;
            }
        }
        line += ins;
    }
    out
}

/// The shortest edit script turning `a` into `b` (Myers, O((N+M)·D)), or
/// `None` when it needs more than `MAX_EDITS` edits.
fn myers(a: &[u64], b: &[u64]) -> Option<Vec<Edit>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let limit = max.min(MAX_EDITS);
    let offset = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    // trace[d] is the slice of `v` for diagonals -(d+1)..=d+1 as it stood
    // before step d — exactly what backtracking through step d reads.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let at = |k: isize| (k + offset) as usize;

    for d in 0..=limit as isize {
        trace.push(v[at(-d - 1)..=at(d + 1)].to_vec());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]) {
                v[at(k + 1)]
            } else {
                v[at(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Edit> {
    let mut script = Vec::new();
    let (mut x, mut y) = (n, m);
    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let get = |k: isize| v[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && get(k - 1) < get(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = get(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            script.push(Edit::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            script.push(if x == prev_x {
                Edit::Insert
            } else {
                Edit::Delete
            });
        }
        x = prev_x;
        y = prev_y;
    }
    script.reverse();
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(lines: &[&str]) -> Vec<u64> {
        lines.iter().map(|l| hash(l.as_bytes())).collect()
    }

    #[test]
    fn whodidit_output_parses_to_the_sealed_lines() {
        let out = "38fa3b91 ancient-dragon-38fa3b91 (javan <j@x.com>) 1)  a\n\
                   \x20                                               2)  b 3)  c\n\
                   ca550c0b long-waterfall-ca550c0b (javan <j@x.com>) 3)    lead\n\
                   \x20                                               4)  \ttab\n\
                   \x20                                               5)  \n";
        assert_eq!(
            parse_whodidit(out),
            vec!["a", "b 3)  c", "  lead", "\ttab", ""]
        );
    }

    #[test]
    fn marks_added_modified_and_deleted_lines() {
        let base = h(&["a", "b", "c", "d", "e"]);
        // b changed, x inserted after c, e deleted.
        let cur = h(&["a", "B", "c", "x", "d"]);
        let m = marks(&base, &cur);
        assert_eq!(
            m,
            vec![
                Mark::None,
                Mark::Modified,
                Mark::None,
                Mark::Added,
                Mark::DeletedAbove
            ]
        );
    }

    #[test]
    fn a_deleted_middle_marks_the_line_below() {
        let m = marks(&h(&["a", "b", "c"]), &h(&["a", "c"]));
        assert_eq!(m, vec![Mark::None, Mark::DeletedAbove]);
    }

    #[test]
    fn untracked_files_are_all_added_and_hunks_are_found() {
        let mut state = DocState {
            base: Base::Untracked,
            ..DocState::default()
        };
        state.recompute(h(&["x", "y"]), 3);
        assert_eq!(state.marks, vec![Mark::Added, Mark::Added]);
        assert_eq!(state.stats, (2, 0, 0));
        assert_eq!(state.hunk_starts(), vec![0]);

        state.base = Base::Tracked(h(&["a", "b", "c", "d"]));
        state.recompute(h(&["a", "B", "c", "d", "e"]), 4);
        assert_eq!(state.hunk_starts(), vec![1, 4]);
        assert_eq!(state.marks_revision, Some(4));
    }

    #[test]
    fn a_rope_line_hashes_like_its_text_without_its_newline() {
        let rope = ropey::Rope::from_str("alpha\nbeta\r\n");
        assert_eq!(hash_line(rope.line(0)), hash(b"alpha"));
        assert_eq!(hash_line(rope.line(1)), hash(b"beta"));
    }

    /// The guard that keeps a rewritten file from stalling the editor: past
    /// the edit ceiling the whole differing middle is simply marked.
    #[test]
    fn a_wholesale_rewrite_falls_back_instead_of_grinding() {
        let base: Vec<u64> = (0..3000)
            .map(|i| hash(format!("old {i}").as_bytes()))
            .collect();
        let cur: Vec<u64> = (0..3000)
            .map(|i| hash(format!("new {i}").as_bytes()))
            .collect();
        let start = std::time::Instant::now();
        let m = marks(&base, &cur);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(100),
            "the diff has to stay out of the way of typing: {:?}",
            start.elapsed()
        );
        assert_eq!(m.len(), 3000);
        assert!(m.iter().all(|&mark| mark != Mark::None));
    }

    #[test]
    fn identical_files_have_no_marks() {
        let base = h(&["a", "b"]);
        assert_eq!(marks(&base, &base), vec![Mark::None, Mark::None]);
    }
}
