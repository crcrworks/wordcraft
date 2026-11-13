//! What the session ran, for a host that sends the commands instead of the document.
//!
//! bace embeds the engine in a window and mirrors that window's document in another client's
//! window by sending the commands, not the file (`docs/decisions/0032-wordcraft-journal.md` in
//! `crcrworks/bace`). Every user-visible edit is already a command, so the record of them is what
//! travels: the host takes it here and the other session runs the same steps.
//!
//! This is **not** the undo history. Undo and redo are whole-document snapshots
//! (`Session::history`), so a session cannot say which commands they correspond to. Their document
//! change is sent as a [`REVERT`] step instead: the difference between the snapshot and the
//! document, as block patches (see [`revert`]). Repeat is different: it re-runs the last command,
//! and that command is journaled like any other. Undo of an edit no journaled command made — a
//! table, an equation, a picture, page setup — has no such difference the log can carry, and
//! **cuts** the log instead: the host takes a [`Taken`] with `cut` set, throws away what it had not
//! sent, and offers the document itself as the log's next base. The same happens for an edit whose
//! command is not marked [`journal`](crate::CommandSpec) (a table, a picture, a paste from the
//! clipboard — one id and its params do not describe it) and for one that reads state the log
//! cannot carry (a column block, whole table cells, an equation being edited).
//!
//! ## What a step is
//!
//! A [`Step`] is a command id and its params, as [`Session::run`](crate::Session::run) took it.
//! WordCraft's edit commands work on the *selection* (type here, bold that), and the selection is
//! each session's own: the other session's caret can be anywhere. So in front of every command the
//! log records the state it read — the selection and the formatting waiting at the caret — as a
//! [`STATE`] step. Applied in order, the pair reproduces the author's edit wherever the reader's
//! caret was, and the reader's own caret and pending formatting are put back afterwards
//! (`Session::apply_journal`). A [`Step`] count is therefore twice the command count.
//!
//! ## Entries
//!
//! Recorded steps are grouped into entries: one entry per run of edits that belong together — the
//! keystrokes of one typing group, the frames of one drag. The group is not folded into one step:
//! running `text.insert "a"` then `"b"` and running `"ab"` are not the same edit (smart quotes,
//! AutoFormat and AutoCorrect act per keystroke), so the steps stay as the session ran them and
//! only the entry count stays small.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::Selection;
use crate::cmd::pos_json;
use wordcraft_doc::props::CharProps;
use wordcraft_doc::{Block, Document, StoryRef, Table};

/// How many entries the log keeps before it drops the oldest.
///
/// The same cap as GridCraft's journal, which bace already knows how to fall back from
/// (`docs/decisions/0027-command-transport.md` 決定 9).
pub const JOURNAL_MAX: usize = 10_000;

/// How many entries a drop takes.
pub const JOURNAL_DROP: usize = 5_000;

/// The step in front of a recorded command: the selection it worked on and the formatting waiting
/// at the caret (`{"anchor": Pos, "focus": Pos, "pending": CharProps | null}`). Not a command in
/// the registry; [`Session::apply_journal`](crate::Session::apply_journal) takes it.
pub const STATE: &str = "journal.state";

/// The step Undo and Redo record instead of a command: block patches that take the document from
/// the snapshot to the document the user sees now (`{"patches": [...]}`, see [`revert`]). Not a
/// command in the registry; [`Session::apply_journal`](crate::Session::apply_journal) takes it.
/// The patches are absolute changes to a story's blocks, not inverse commands: they do not read
/// the selection, so the session that applies them keeps its own caret.
pub const REVERT: &str = "journal.revert";

/// One command as `Session::run` took it.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// The command id (`text.insert`).
    pub id: String,
    /// The params it was given (`{"text": "a"}`).
    pub params: Value,
}

impl Step {
    /// A step for a command run with `params`.
    pub fn new(id: impl Into<String>, params: Value) -> Self {
        Step { id: id.into(), params }
    }

    /// The same step as the transport's `(id, params)` pair.
    pub fn into_pair(self) -> (String, Value) {
        (self.id, self.params)
    }
}

/// What a host takes from the log.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Taken {
    /// The steps recorded since the last take, oldest first.
    pub steps: Vec<Step>,
    /// The log cannot be replayed from its beginning any more (an undo the log cannot carry, a
    /// command that is not a step, or the cap dropping the oldest entries). `steps` is empty and
    /// the host has to offer the document as the log's new base.
    pub cut: bool,
}

/// Where the log had got to, so a caller that runs commands and then takes them back without a
/// trace can take them back out of the log too (`Session::edit_snapshot` / `Session::restore`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    entries: usize,
    /// The steps in the last entry the mark leaves, so a command that joined it is dropped too.
    steps: usize,
    cut: bool,
}

/// Whether the command about to run belongs with the last entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Joins {
    /// It starts an entry of its own.
    #[default]
    No,
    /// Another keystroke of the open typing group: it goes in the same entry as the ones before
    /// it (as its own steps — see the module docs).
    Typing,
    /// A later frame of the same drag: same entry.
    Drag,
}

/// A run of steps recorded together. The next command may join it (`open`).
#[derive(Default)]
struct Entry {
    steps: Vec<Step>,
    open: Joins,
}

/// The commands a session ran, for a host that mirrors the document elsewhere.
#[derive(Default)]
pub struct Journal {
    entries: Vec<Entry>,
    /// A command that is not one step ran: the host needs a new base.
    cut: bool,
}

impl Journal {
    /// Records `id`/`params`, a command that changed the document at `sel` with `pending` waiting
    /// at the caret.
    ///
    /// `joins` says whether it continues the last entry. The state step goes in front of the
    /// command so a reader that applies the entry runs it at the same place and with the same
    /// formatting the author had.
    pub(crate) fn record(&mut self, id: &str, params: &Value, sel: &Selection, pending: Option<&CharProps>, joins: Joins) {
        let continues = !self.entries.is_empty()
            && match joins {
                // A later frame of a drag is part of the step before (`join_next_undo` says so).
                Joins::Drag => true,
                // Another keystroke continues the typing run, if the entry is still open to one.
                Joins::Typing => self.entries.last().is_some_and(|entry| entry.open == joins),
                Joins::No => false,
            };
        if !continues {
            self.entries.push(Entry::default());
        }
        let Some(entry) = self.entries.last_mut() else { return };
        entry.open = joins;
        let state = json!({"anchor": pos_json(&sel.anchor), "focus": pos_json(&sel.focus), "pending": pending});
        entry.steps.push(Step::new(STATE, state));
        entry.steps.push(Step::new(id, params.clone()));
        self.drop_past_cap();
    }

    /// Records the patches that undo (or redo) made, in an entry of their own.
    ///
    /// A revert step is not a command and carries no state step: it names blocks by path and
    /// index, so the session that applies it changes the same blocks and leaves its own caret
    /// alone.
    pub(crate) fn record_revert(&mut self, params: Value) {
        self.entries.push(Entry { steps: vec![Step::new(REVERT, params)], open: Joins::No });
        self.drop_past_cap();
    }

    /// Takes everything recorded since the last call.
    ///
    /// A cut drops the steps as well: they are inside the document the host is about to offer as
    /// the next base, so sending them would apply them twice.
    pub fn take(&mut self) -> Taken {
        if self.cut {
            self.clear();
            return Taken { steps: Vec::new(), cut: true };
        }
        let steps = self.entries.drain(..).flat_map(|entry| entry.steps).collect();
        Taken { steps, cut: false }
    }

    /// The commands cannot be replayed from the start of the log any more: an undo whose document
    /// change is not a patch (a table, an equation, a picture, page setup), or an edit that is not
    /// a step. The next [`Self::take`] reports a cut once, and drops the steps with it.
    ///
    /// The entries stay until that take, so a caller that takes a command back out of the document
    /// (`Session::restore`) can rewind the cut away along with it.
    pub(crate) fn cut(&mut self) {
        self.cut = true;
    }

    /// Drops what is here without asking for a base: the steps are inside a document the host just
    /// handed out, or the app replaced its document.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.cut = false;
    }

    /// The log's position now, for [`Self::rewind`].
    pub(crate) fn mark(&self) -> Mark {
        Mark { entries: self.entries.len(), steps: self.entries.last().map_or(0, |entry| entry.steps.len()), cut: self.cut }
    }

    /// Drops what was recorded after `mark` and puts the cut flag back: the caller took the
    /// commands' effects out of the document (`Session::restore`), so they must not travel.
    pub(crate) fn rewind(&mut self, mark: Mark) {
        let Mark { entries, steps, cut } = mark;
        self.entries.truncate(entries);
        if let Some(entry) = self.entries.last_mut() {
            entry.steps.truncate(steps);
        }
        self.cut = cut;
    }

    /// How many entries the log holds (the number [`JOURNAL_MAX`] counts).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops the oldest entries past the cap and marks the log for a base.
    fn drop_past_cap(&mut self) {
        if self.entries.len() <= JOURNAL_MAX {
            return;
        }
        self.entries.drain(..JOURNAL_DROP.min(self.entries.len()));
        self.cut = true;
    }
}

/// The patches that take `from` to `to`, if the difference is one a journal can carry.
///
/// Undo and Redo move the document to a whole-document snapshot. Sending that snapshot
/// would be a new base (`Taken::cut`), which makes the other client reopen the document and lose
/// its undo history (`docs/decisions/0046-undo-as-differences.md` in `crcrworks/bace`). The
/// difference between the two documents is usually small: typing changed one paragraph, formatting
/// changed one paragraph's properties. This turns it into block patches.
///
/// `None` when the change is not just blocks: a table's shape or formatting, an equation, a
/// picture's media, page setup, styles, comments or any other document property. Those edits are
/// not journaled either (`CommandSpec.journal`), and Undo of them keeps the base fallback.
///
/// A patch is `{"story": StoryRef, "path": [u32], "at": n, "remove": n, "blocks": [Block]}`.
/// `path` is the containing cell path (`[]` for a story, `[block, row, cell]` per table level),
/// `at` and `remove` are an index and a count in that container, and `blocks` is what replaces
/// them. The patches are ordered outermost first, so applying them in order reaches `to` even
/// when an earlier patch changes a table another patch then edits.
pub(crate) fn revert(from: &Document, to: &Document) -> Option<Value> {
    if !same_outside_stories(from, to) {
        return None;
    }
    let mut patches = Vec::new();
    if !diff_blocks(&from.body, &to.body, 0, StoryRef::Body, &[], &mut patches) {
        return None;
    }
    for id in from.parts.keys() {
        let story = StoryRef::Part(*id);
        let (Some(before), Some(after)) = (from.story(story), to.story(story)) else { return None };
        if !diff_blocks(before, after, 0, story, &[], &mut patches) {
            return None;
        }
    }
    Some(json!({ "patches": patches }))
}

/// Whether every document field outside the stories' blocks is the same. The blocks are cleared
/// in copies and compared by [`Document::same_content`], which lists every field, so a new field
/// cannot silently become patchable.
fn same_outside_stories(a: &Document, b: &Document) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.body.clear();
    b.body.clear();
    for part in a.parts.values_mut() {
        part.blocks.clear();
    }
    for part in b.parts.values_mut() {
        part.blocks.clear();
    }
    a.same_content(&b)
}

/// Appends the patches that turn `from` into `to`. `base` is where `from[0]` sits in the
/// container, so a patch made after a split still names the right block. Returns false when a
/// change is not a block patch (a table's shape, or a table block added or removed).
fn diff_blocks(from: &[Arc<Block>], to: &[Arc<Block>], base: usize, story: StoryRef, path: &[u32], patches: &mut Vec<Value>) -> bool {
    if from == to {
        return true;
    }
    let shorter = from.len().min(to.len());
    let mut prefix = 0;
    while prefix < shorter && from[prefix] == to[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < shorter - prefix && from[from.len() - 1 - suffix] == to[to.len() - 1 - suffix] {
        suffix += 1;
    }
    let from = &from[prefix..from.len() - suffix];
    let to = &to[prefix..to.len() - suffix];
    let base = base + prefix;
    if from.len() == to.len() {
        let mut i = 0;
        while i < from.len() {
            match (&*from[i], &*to[i]) {
                (Block::Para(_), Block::Para(_)) => {
                    let start = i;
                    i += 1;
                    while i < from.len() && matches!((&*from[i], &*to[i]), (Block::Para(_), Block::Para(_))) {
                        i += 1;
                    }
                    patches.push(patch(story, path, base + start, i - start, &to[start..i]));
                }
                (Block::Table(before), Block::Table(after)) => {
                    if !same_table_shape(before, after) {
                        return false;
                    }
                    for (row, (before, after)) in before.rows.iter().zip(&after.rows).enumerate() {
                        for (cell, (before, after)) in before.cells.iter().zip(&after.cells).enumerate() {
                            let mut cell_path = path.to_vec();
                            cell_path.extend([(base + i) as u32, row as u32, cell as u32]);
                            if !diff_blocks(&before.blocks, &after.blocks, 0, story, &cell_path, patches) {
                                return false;
                            }
                        }
                    }
                    i += 1;
                }
                _ => return false,
            }
        }
        return true;
    }
    // The block count changed: a paragraph was split, merged, inserted or removed. An unchanged
    // block (same allocation) inside the changed run is an anchor; the runs before and after it
    // can be patches on their own. Without one, a table in the run would have to be added or
    // removed, which a patch does not describe.
    let mut anchors = HashMap::new();
    for (index, block) in to.iter().enumerate() {
        anchors.entry(Arc::as_ptr(block) as usize).or_insert(index);
    }
    if let Some((at, with)) = from.iter().enumerate().find_map(|(at, block)| anchors.get(&(Arc::as_ptr(block) as usize)).map(|with| (at, *with))) {
        return diff_blocks(&from[..at], &to[..with], base, story, path, patches)
            && diff_blocks(&from[at + 1..], &to[with + 1..], base + with + 1, story, path, patches);
    }
    if from.iter().chain(to).any(|block| matches!(&**block, Block::Table(_))) {
        return false;
    }
    patches.push(patch(story, path, base, from.len(), to));
    true
}

fn patch(story: StoryRef, path: &[u32], at: usize, remove: usize, blocks: &[Arc<Block>]) -> Value {
    json!({ "story": story, "path": path, "at": at, "remove": remove, "blocks": blocks })
}

/// Whether two tables have the same shape and table/row/cell properties: the same rows and cells,
/// with the same widths, merges, borders and controls. Only their cells' blocks may differ (that
/// is what a patch can reach). Any other change — a column inserted, a row's height, a cell's
/// span — is not a block patch.
fn same_table_shape(a: &Table, b: &Table) -> bool {
    a.props == b.props
        && a.grid == b.grid
        && a.controls == b.controls
        && a.rows.len() == b.rows.len()
        && a.rows.iter().zip(&b.rows).all(|(a, b)| {
            a.props == b.props
                && a.controls == b.controls
                && a.cells.len() == b.cells.len()
                && a.cells.iter().zip(&b.cells).all(|(a, b)| a.props == b.props && a.controls == b.controls)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wordcraft_doc::{Document, Pos, StoryRef};

    use crate::Session;

    fn session(text: &str) -> Session {
        let mut s = Session::new(Document::new());
        if !text.is_empty() {
            s.run("text.insert", &json!({"text": text})).unwrap();
            s.take_journal();
        }
        s
    }

    fn select(s: &mut Session, from: usize, to: usize) {
        s.sel = Selection { anchor: Pos::body(0, from), focus: Pos::body(0, to) };
    }

    /// The commands a run journaled, without the state steps in front of them.
    fn ids(taken: &Taken) -> Vec<&str> {
        taken.steps.iter().map(|step| step.id.as_str()).filter(|id| *id != STATE).collect()
    }

    /// What a host sends: the steps as `(id, params)` pairs.
    fn pairs(s: &mut Session) -> Vec<(String, Value)> {
        s.take_journal().steps.into_iter().map(Step::into_pair).collect()
    }

    /// What the other session does with them; returns how many commands ran.
    fn apply(s: &mut Session, taken: &Taken) -> usize {
        let steps: Vec<(String, Value)> = taken.steps.iter().cloned().map(Step::into_pair).collect();
        s.apply_journal(&steps)
    }

    #[test]
    fn typing_is_one_entry_with_the_state_and_a_step_per_keystroke() {
        let mut s = session("");
        for ch in ["a", "b", "c"] {
            s.run("text.insert", &json!({"text": ch})).unwrap();
        }
        assert_eq!(s.journal_entries(), 1, "a typing run is one entry");
        let taken = s.take_journal();
        assert!(!taken.cut);
        assert_eq!(ids(&taken), ["text.insert", "text.insert", "text.insert"]);
        let typed: Vec<Value> = taken.steps.iter().filter(|step| step.id == "text.insert").map(|step| step.params["text"].clone()).collect();
        assert_eq!(typed, [json!("a"), json!("b"), json!("c")]);
        // Each command has the state it read in front of it.
        assert_eq!(taken.steps.len(), 6);
        assert_eq!(taken.steps[0].id, STATE);
        assert!(taken.steps[0].params.get("anchor").is_some());
        // Nothing is left after a take.
        assert!(s.take_journal().steps.is_empty());
    }

    #[test]
    fn typing_after_a_caret_move_stays_one_entry() {
        let mut s = session("");
        s.run("text.insert", &json!({"text": "a"})).unwrap();
        s.run("caret.left", &json!({})).unwrap();
        s.run("text.insert", &json!({"text": "b"})).unwrap();
        // A pure command is not an edit: it neither joins nor cuts the run.
        assert_eq!(s.journal_entries(), 1);
        let taken = s.take_journal();
        assert_eq!(ids(&taken), ["text.insert", "text.insert"]);
        // The state in front of each keystroke says where it went.
        assert_eq!(taken.steps[2].id, STATE);
        assert_eq!(taken.steps[2].params["focus"]["off"], json!(0));
        assert_eq!(s.doc.plain_text(StoryRef::Body), "ba");
    }

    #[test]
    fn a_drag_is_one_entry() {
        let mut s = session("abc");
        select(&mut s, 0, 3);
        s.run("format.size", &json!({"size": 12})).unwrap();
        s.join_next_undo();
        s.run("format.size", &json!({"size": 14})).unwrap();
        s.join_next_undo();
        s.run("format.size", &json!({"size": 16})).unwrap();
        assert_eq!(s.journal_entries(), 1, "later frames of a drag stay in the entry it opened");
        let taken = s.take_journal();
        let sizes: Vec<Value> = taken.steps.iter().filter(|step| step.id == "format.size").map(|step| step.params["size"].clone()).collect();
        assert_eq!(sizes, [json!(12), json!(14), json!(16)], "each frame is its own step: the last one wins when replayed");
    }

    /// Undo and Redo no longer cut the log: the step they take is sent as the block
    /// patches between the snapshot and the document, and the other session applies them. Its
    /// own caret and undo stack are left alone, and the author still has the steps below the one
    /// it undid.
    #[test]
    fn undo_and_redo_send_the_difference_and_keep_the_undo_stack() {
        let mut author = session("");
        let mut reader = session("");
        author.run("text.insert", &json!({"text": "a"})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 1);
        author.run("format.bold", &json!({})).unwrap();
        author.run("text.insert", &json!({"text": "b"})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 2);
        assert_eq!(reader.doc.plain_text(StoryRef::Body), "ab");
        let depth = author.undo_depth();

        author.run("edit.undo", &json!({})).unwrap();
        assert_eq!(author.undo_depth(), depth - 1);
        assert!(author.can_undo(), "the steps below the one undone are still there");
        let taken = author.take_journal();
        assert!(!taken.cut);
        assert_eq!(ids(&taken), [REVERT]);
        assert_eq!(apply(&mut reader, &taken), 1);
        assert_eq!(reader.doc.plain_text(StoryRef::Body), "a");
        assert_eq!(reader.doc.plain_text(StoryRef::Body), author.doc.plain_text(StoryRef::Body));

        // The next step only changed the pending formatting: no block, so nothing travels and
        // no base is asked for.
        author.run("edit.undo", &json!({})).unwrap();
        let taken = author.take_journal();
        assert!(!taken.cut);
        assert!(taken.steps.is_empty());

        author.run("edit.undo", &json!({})).unwrap();
        let taken = author.take_journal();
        assert!(!taken.cut);
        assert_eq!(ids(&taken), [REVERT]);
        assert_eq!(apply(&mut reader, &taken), 1);
        assert_eq!(reader.doc.plain_text(StoryRef::Body), "");
        assert_eq!(reader.doc.plain_text(StoryRef::Body), author.doc.plain_text(StoryRef::Body));

        author.run("edit.redo", &json!({})).unwrap();
        let taken = author.take_journal();
        assert!(!taken.cut);
        assert_eq!(ids(&taken), [REVERT]);
        assert_eq!(apply(&mut reader, &taken), 1);
        assert_eq!(reader.doc.plain_text(StoryRef::Body), "a");
        assert_eq!(reader.doc.plain_text(StoryRef::Body), author.doc.plain_text(StoryRef::Body));
    }

    /// A paragraph split changes the number of blocks. Its undo merges them back, which is a
    /// patch over a range of blocks, not a new base: the other session converges too.
    #[test]
    fn undo_of_a_paragraph_split_is_a_patch() {
        let mut author = session("");
        let mut reader = session("");
        author.run("text.insert", &json!({"text": "a"})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 1);
        author.run("text.newParagraph", &json!({})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 1);
        author.run("text.insert", &json!({"text": "b"})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 1);

        for _ in 0..3 {
            author.run("edit.undo", &json!({})).unwrap();
            let taken = author.take_journal();
            assert!(!taken.cut);
            if !taken.steps.is_empty() {
                assert_eq!(ids(&taken), [REVERT]);
                assert_eq!(apply(&mut reader, &taken), 1);
            }
            assert_eq!(reader.doc.plain_text(StoryRef::Body), author.doc.plain_text(StoryRef::Body));
        }
        assert_eq!(author.doc.plain_text(StoryRef::Body), "");
    }

    /// An undo that arrived is one step in the reader's own history, so its ⌘Z takes that undo
    /// back and sends the inverse: the two sessions can undo and redo each other's work without
    /// either becoming the other's base.
    #[test]
    fn a_received_undo_is_undoable_and_sends_the_inverse() {
        let mut author = session("");
        let mut reader = session("");
        author.run("text.insert", &json!({"text": "a"})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 1);
        author.run("format.bold", &json!({})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 1);
        author.run("text.insert", &json!({"text": "b"})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 1);
        let depth = reader.undo_depth();

        author.run("edit.undo", &json!({})).unwrap();
        let taken = author.take_journal();
        assert_eq!(apply(&mut reader, &taken), 1);
        assert_eq!(reader.undo_depth(), depth + 1);
        assert_eq!(reader.doc.plain_text(StoryRef::Body), "a");

        reader.run("edit.undo", &json!({})).unwrap();
        assert_eq!(reader.doc.plain_text(StoryRef::Body), "ab");
        let taken = reader.take_journal();
        assert!(!taken.cut);
        assert_eq!(ids(&taken), [REVERT]);
        assert_eq!(apply(&mut author, &taken), 1);
        assert_eq!(author.doc.plain_text(StoryRef::Body), "ab");
        assert_eq!(author.doc.plain_text(StoryRef::Body), reader.doc.plain_text(StoryRef::Body));
    }

    /// Repeat re-runs the last command, so what it journals is that command, not a cut: the
    /// other session runs the same command and the documents stay together.
    #[test]
    fn repeat_sends_the_command_it_repeats() {
        let mut author = session("");
        let mut reader = session("");
        author.run("text.insert", &json!({"text": "a"})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 1);

        author.run("text.insert", &json!({"text": "b"})).unwrap();
        assert_eq!(reader.apply_journal(&pairs(&mut author)), 1);
        author.run("edit.repeat", &json!({})).unwrap();
        let taken = author.take_journal();
        assert!(!taken.cut);
        assert_eq!(ids(&taken), ["text.insert"]);
        assert_eq!(apply(&mut reader, &taken), 1);
        assert_eq!(reader.doc.plain_text(StoryRef::Body), "abb");
        assert_eq!(reader.doc.plain_text(StoryRef::Body), author.doc.plain_text(StoryRef::Body));
    }

    /// Undo of an edit no journaled command made (a table) has no patch to send. It cuts the log,
    /// and the host offers the document as the next base, as before.
    #[test]
    fn undo_of_an_edit_that_is_not_a_step_cuts_the_log() {
        let mut s = session("");
        s.run("insert.table", &json!({"rows": 1, "cols": 1})).unwrap();
        assert!(s.take_journal().cut, "the table itself is not a step");
        s.run("edit.undo", &json!({})).unwrap();
        let taken = s.take_journal();
        assert!(taken.cut, "the table's undo is a whole-document snapshot");
        assert!(taken.steps.is_empty(), "steps inside the new base would apply twice");
        assert!(!s.take_journal().cut, "the host is asked for a base once");
    }

    #[test]
    fn an_edit_that_is_not_a_step_cuts_the_log() {
        let mut s = session("");
        s.run("text.insert", &json!({"text": "a"})).unwrap();
        // A page break is insert.pageBreak/insert.*: not one command a step describes.
        s.run("insert.pageBreak", &json!({})).unwrap();
        let taken = s.take_journal();
        assert!(taken.cut);
        assert!(taken.steps.is_empty());
    }

    #[test]
    fn a_block_selection_cuts_the_log() {
        let mut s = session("hello");
        s.also_selected.push(Pos::body(0, 0));
        s.run("format.bold", &json!({})).unwrap();
        assert!(s.take_journal().cut, "the state the command read is not in the step");
    }

    #[test]
    fn a_command_that_changes_nothing_is_not_recorded() {
        let mut s = session("");
        s.run("format.state", &json!({})).unwrap();
        s.run("view.zoom", &json!({"percent": 120})).unwrap();
        assert_eq!(s.journal_entries(), 0);
        let taken = s.take_journal();
        assert!(!taken.cut);
        assert!(taken.steps.is_empty());
    }

    #[test]
    fn nothing_is_journaled_while_applying_another_sessions_steps() {
        let mut author = session("");
        let mut reader = session("");
        author.run("text.insert", &json!({"text": "a"})).unwrap();
        let steps = pairs(&mut author);
        assert!(!steps.is_empty());
        assert_eq!(reader.apply_journal(&steps), 1);
        assert_eq!(reader.journal_entries(), 0, "a step replayed here is not journaled in turn");
        let taken = reader.take_journal();
        assert!(!taken.cut);
        assert!(taken.steps.is_empty());
    }

    #[test]
    fn another_session_replays_the_edits_and_keeps_its_own_caret() {
        let mut author = session("hello world");
        let mut reader = session("hello world");
        select(&mut reader, 6, 11);
        let own_sel = reader.sel.clone();

        select(&mut author, 0, 5);
        author.run("format.bold", &json!({})).unwrap();
        select(&mut author, 11, 11);
        author.run("text.insert", &json!({"text": "!"})).unwrap();

        let steps = pairs(&mut author);
        assert_eq!(
            ids(&Taken { steps: steps.iter().map(|(id, params)| Step::new(id.clone(), params.clone())).collect(), cut: false }),
            ["format.bold", "text.insert"]
        );
        assert_eq!(reader.apply_journal(&steps), 2);
        assert_eq!(reader.doc.plain_text(StoryRef::Body), author.doc.plain_text(StoryRef::Body));
        assert_eq!(reader.doc.plain_text(StoryRef::Body), "hello world!");
        let para = reader.doc.para_at(&Pos::body(0, 0)).unwrap();
        assert_eq!(para.props_of_char(0).bold, Some(true), "the bold landed on the author's selection");
        assert_eq!(para.props_of_char(5).bold, None);
        assert_eq!(reader.sel, own_sel, "the reader's own caret is where it was");
        assert!(reader.take_journal().steps.is_empty());
    }

    #[test]
    fn pending_formatting_travels_with_the_keystrokes() {
        let mut author = session("");
        let mut reader = session("");
        select(&mut author, 0, 0);
        author.run("format.bold", &json!({})).unwrap();
        author.run("text.insert", &json!({"text": "a"})).unwrap();
        let steps = pairs(&mut author);

        // The reader has formatting of its own waiting at its caret; the author's steps must not
        // pick it up, and it must still be there afterwards.
        reader.run("format.italic", &json!({})).unwrap();
        reader.take_journal();
        assert_eq!(reader.apply_journal(&steps), 2);
        let para = reader.doc.para_at(&Pos::body(0, 0)).unwrap();
        assert_eq!(para.props_of_char(0).bold, Some(true));
        assert_eq!(para.props_of_char(0).italic, None);
        assert_eq!(reader.pending.as_ref().and_then(|p| p.italic), Some(true));
    }

    /// A dialog that runs its commands as one step and puts the document back when one fails
    /// (`Session::restore`) must not leave them in the log: the other client would run edits the
    /// author no longer has (0032 決定 6).
    #[test]
    fn restoring_a_snapshot_takes_the_commands_back_out_of_the_log() {
        let mut s = session("");
        s.run("text.insert", &json!({"text": "a"})).unwrap();
        let snapshot = s.edit_snapshot();

        s.run("text.insert", &json!({"text": "b"})).unwrap();
        s.run("insert.pageBreak", &json!({})).unwrap();
        s.restore(snapshot);

        let taken = s.take_journal();
        assert!(!taken.cut, "the commands were taken back, so nothing needs a base");
        assert_eq!(ids(&taken), ["text.insert"], "only the keystroke before the snapshot is left");
        assert_eq!(s.doc.plain_text(StoryRef::Body), "a");
    }

    #[test]
    fn the_cap_drops_the_oldest_entries_and_asks_for_a_base() {
        let mut journal = Journal::default();
        let sel = Selection::caret(Pos::body(0, 0));
        for _ in 0..=JOURNAL_MAX {
            journal.record("text.insert", &json!({"text": "a"}), &sel, None, Joins::No);
        }
        assert_eq!(journal.len(), JOURNAL_MAX + 1 - JOURNAL_DROP);
        let taken = journal.take();
        assert!(taken.cut, "the host has to offer a base: the steps before the drop are gone");
        assert!(taken.steps.is_empty());
        assert!(journal.is_empty());
    }
}
