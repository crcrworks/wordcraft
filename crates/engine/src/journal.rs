//! What the session ran, for a host that sends the commands instead of the document.
//!
//! bace embeds the engine in a window and mirrors that window's document in another client's
//! window by sending the commands, not the file (`docs/decisions/0032-wordcraft-journal.md` in
//! `crcrworks/bace`). Every user-visible edit is already a command, so the record of them is what
//! travels: the host takes it here and the other session runs the same steps.
//!
//! This is **not** the undo history. Undo, redo and repeat are whole-document snapshots
//! (`Session::history`), so a session cannot say which commands they correspond to. Running one
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

use serde_json::{Value, json};

use crate::Selection;
use crate::cmd::pos_json;
use wordcraft_doc::props::CharProps;

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
    /// The log cannot be replayed from its beginning any more (undo, a command that is not a
    /// step, or the cap dropping the oldest entries). `steps` is empty and the host has to offer
    /// the document as the log's new base.
    pub cut: bool,
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

    /// The commands cannot be replayed from the start of the log any more: undo, redo, repeat, or
    /// an edit that is not a step. The next [`Self::take`] reports a cut once.
    pub(crate) fn cut(&mut self) {
        self.clear();
        self.cut = true;
    }

    /// Drops what is here without asking for a base: the steps are inside a document the host just
    /// handed out, or the app replaced its document.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.cut = false;
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

    #[test]
    fn undo_redo_and_repeat_cut_the_log() {
        let mut s = session("");
        s.run("text.insert", &json!({"text": "a"})).unwrap();
        s.run("text.insert", &json!({"text": "b"})).unwrap();
        s.run("edit.undo", &json!({})).unwrap();
        let taken = s.take_journal();
        assert!(taken.cut, "undo is a whole-document snapshot: the log cannot be replayed from its start");
        assert!(taken.steps.is_empty(), "steps inside the new base would apply twice");
        assert!(!s.take_journal().cut, "the host is asked for a base once");

        s.run("edit.redo", &json!({})).unwrap();
        assert!(s.take_journal().cut);

        s.run("text.insert", &json!({"text": "c"})).unwrap();
        s.run("edit.repeat", &json!({})).unwrap();
        assert!(s.take_journal().cut);
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
