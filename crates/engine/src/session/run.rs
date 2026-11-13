// bace: journal; applying a patch is undoable and evicts the oldest step like any other edit.
use super::doc_state::{ColumnBlock, MAX_UNDO, Selection, Undo};
use super::ime_state::ImeCaretState;
use super::snapshot_types::RunSnapshot;
use super::{CharProps, CmdError, Document, Fragment, ImeEditOutcome, Path, Pos, Session, StoryRef, Value, document_id_mark};
use crate::CommandSpec;
// bace: journal; a patch replaces `Arc<Block>` values.
use std::sync::Arc;
use wordcraft_doc::Block;

struct SuccessContext<'a> {
    id: &'a str,
    spec: &'a CommandSpec,
    before_doc: &'a Option<RunSnapshot>,
    pending_before: &'a Option<Option<CharProps>>,
    document_id_before: u64,
    replaces_document: bool,
    dirty_before: Option<bool>,
    rev_restore: Option<u64>,
    pushed: bool,
    // bace: journal; the step this command will mark.
    /// The history step this command is about to change (the checkpoint it pushed, or the step
    /// a joined command merges into). `finish_success` marks it journaled when the command and
    /// its document change are both journalable.
    journal_step: Option<Arc<Undo>>,
    mutates: bool,
    ime_caret_before: ImeCaretState,
    rev_before: u64,
    sel_before: &'a Selection,
    // bace: journal
    params: &'a Value,
    joins: crate::journal::Joins,
    journal_before: &'a JournalBefore,
}

// bace: what a command read, for the journal.
/// The state a command is about to read, captured by [`Session::run`] for
/// [`Session::note_journal`] (`crate::journal`).
struct JournalBefore {
    sel: Selection,
    pending: Option<CharProps>,
    rev: u64,
    document: u64,
    /// The command reads a column block, whole table cells, an equation being edited or extra
    /// selected objects: state a step cannot carry, so the log is cut instead of recording it.
    blocked: bool,
}

// bace: what replaying another session's steps must leave alone.
/// The session's own editing state, which [`Session::apply_journal`] puts back afterwards: another
/// session's steps move the selection and set the pending formatting to replay their edits there.
struct OwnState {
    sel: Selection,
    pending: Option<CharProps>,
    also_selected: Vec<Pos>,
    goal_x: Option<f32>,
    typing_open: bool,
    join_next: bool,
    edit_marks: Vec<Pos>,
    go_back_step: usize,
}

struct ElsewhereContext {
    home: u64,
    mark: u64,
    before_doc: Option<RunSnapshot>,
    blocks_before: Option<std::collections::BTreeMap<String, Fragment>>,
    default_language_before: Option<String>,
    pending_application_change_before: bool,
}

impl Session {
    fn leave_math_for_command(&mut self, id: &str) {
        if self.math.is_some()
            && (id.starts_with("text.") || id.starts_with("caret.") || id.starts_with("select.") || id.starts_with("edit.paste"))
            && let Some(m) = self.math.take()
        {
            let off = m.at.off + wordcraft_doc::para::OBJ.len_utf8();
            self.sel = Selection::caret(Pos { off, ..m.at });
        }
    }

    /// Whether the command changed nothing: the document, the selection, the formatting
    /// waiting at the caret and the column selection are as they were before it ran.
    ///
    /// A command that changes nothing is not an edit: it adds no undo step, marks no
    /// unsaved changes, forces no relayout and leaves the redo stack alone (#23).
    fn unchanged(&self, doc: &Document, sel: &Selection, column: &Option<ColumnBlock>, pending: &Option<CharProps>) -> bool {
        doc.same_content(&self.doc)
            && *sel == self.sel
            && *column == self.column
            && *pending == self.pending
            && !self.md_source.as_ref().is_some_and(|src| src.is_pending())
    }

    /// A navigated style change is for the Accept or Reject that follows: a command that edits
    /// or moves the selection forgets it.
    fn forget_review_style(&mut self, id: &str, mutates: bool) {
        if (mutates && !matches!(id, "review.accept" | "review.reject")) || id.starts_with("caret.") || id.starts_with("select.") {
            self.review_style = None;
        }
    }

    /// Run a command by id with JSON params (the single entry point for the UI, CLI, MCP and
    /// control channel). Mutating commands are undoable; a failed command leaves the document as
    /// it was; a panic inside a command becomes an error.
    pub fn run(&mut self, id: &str, params: &Value) -> Result<Value, CmdError> {
        if self.md_source.is_some()
            && let Some(mutates) = self.registry.get(id).map(|spec| spec.mutates)
        {
            crate::cmd::md_source::before_command(self, id, mutates)?;
        }
        self.run_command(id, params)
    }

    /// Dispatches `id` through the command registry with transactional rollback on failure,
    /// tracking history, undo checkpoints, and IME outcomes.
    fn run_command(&mut self, id: &str, params: &Value) -> Result<Value, CmdError> {
        // The document the command starts in, and the first id a document it opens can have.
        let home = self.document_id;
        let mark = document_id_mark();
        // Every dispatch starts from an unclassified outcome. Early returns (unknown, disabled,
        // protected) leave it at the default.
        self.ime_edit_outcome = ImeEditOutcome::default();
        // A rolled-back transaction must not leave a successful application-change signal from a
        // nested command that ran before the failure.
        let pending_application_change_before = self.pending_ime_application_change;
        let join = std::mem::take(&mut self.join_next);
        // The equation replacement promise is not a generic undo join: only the command that
        // actually types into the equation may consume it. Any other command discards it.
        let equation_join = std::mem::take(&mut self.equation_join_next);
        // The body replacement promise is likewise scoped: only the `text.insert` at the
        // selection the deletion left may consume it.
        let ime_text_join = self.ime_text_join.take();
        let reg = self.registry.clone();
        let Some(spec) = reg.get(id) else { return Err(CmdError::Unknown(id.to_string())) };
        self.forget_review_style(id, spec.mutates);
        let join = join
            || (equation_join && matches!(id, "equation.type" | "equation.backspace"))
            || (id == "text.insert" && ime_text_join.as_ref().is_some_and(|sel| sel == &self.sel));
        if let Some(why) = (spec.enabled)(self) {
            return Err(CmdError::Disabled(format!("{id}: {why}")));
        }
        // Restrict Editing: refused outright, or allowed as long as it only edits the exceptions.
        let guard = if spec.mutates { crate::cmd::protect::check_before(self, id, params)? } else { false };
        // Quick Parts aren't part of the document, so a refused edit puts them back itself (the Spike).
        let blocks_before = guard.then(|| self.building_blocks.clone());
        if spec.mutates && self.doc.settings.protection.as_deref() == Some("trackedChanges") {
            self.doc.settings.track_changes = true;
        }
        // Macro recording and Repeat.
        let record = !matches!(id, "tools.recordMacro" | "tools.macros" | "edit.undo" | "edit.redo" | "edit.repeat")
            && (spec.mutates || id.starts_with("caret.") || id.starts_with("select."));
        // A password never lands in a recorded macro or Repeat.
        let kept = || {
            let mut p = params.clone();
            if let Some(m) = p.as_object_mut() {
                m.remove("password");
            }
            p
        };
        if record && let Some((_, steps)) = self.recording.as_mut() {
            steps.push((id.to_string(), kept()));
        }
        if spec.mutates && !matches!(id, "edit.undo" | "edit.redo" | "edit.repeat") {
            self.last_command = Some((id.to_string(), kept()));
        }
        // bace: journal; a journaled step carries the selection and the pending formatting and nothing else
        // (`crate::journal`): an edit that reads a block, whole cells, an equation or extra
        // objects cuts the log instead of being recorded.
        let journal_blocked = self.column.is_some() || self.widened.is_some() || self.math.is_some() || !self.also_selected.is_empty();
        // Typing, caret movement and selection outside the equation commands leave the equation.
        self.leave_math_for_command(id);
        // bace: journal the run's commands in one entry.
        // Whether the command is a keystroke of a typing run or a later frame of a drag, so the
        // journal keeps one entry for the run (`crate::journal`).
        let joins = if spec.mutates && matches!(id, "text.insert" | "equation.type") {
            crate::journal::Joins::Typing
        } else if join && spec.mutates {
            crate::journal::Joins::Drag
        } else {
            crate::journal::Joins::No
        };
        // The undo stacks are snapshotted whole, not as a length: commands run nested commands,
        // whose checkpoints, undos and redos (and evictions at the limit) must be put back too.
        let ime_epoch_before = spec.mutates.then_some(self.ime_body_epoch);
        // Commands may leave an equation (a whole-body replacement) even after they fail; put
        // the editing session back with the document.
        let math_before = spec.mutates.then(|| self.math.clone());
        let before_doc: Option<RunSnapshot> = if spec.mutates {
            Some((
                self.doc.clone(),
                self.sel.clone(),
                self.history.clone(),
                self.redo.clone(),
                self.typing_open,
                self.undo_evicted,
                self.column.clone(),
            ))
        } else {
            None
        };
        // The formatting waiting at the caret (⌘B with nothing selected): an undo step restores
        // it, so a command that only changes it is an edit.
        let pending_before = spec.mutates.then(|| self.pending.clone());
        // Which document is open. A command that replaces the document (a mail merge) opens a
        // new one, with a new undo history: it is not an edit of this document, whatever the
        // new document holds.
        let document_id_before = self.document_id;
        // Whether the document has unsaved changes, and the revision its layout is keyed on: a
        // command that changes nothing leaves both as they were, and a failed one leaves no
        // trace.
        let dirty_before = spec.mutates.then_some(self.dirty);
        let rev_restore = spec.mutates.then_some(self.rev);
        // These mutating commands can replace the open document and discard its saved versions.
        // Keep the versions until the command and the protected-region post-check have both
        // succeeded; rejected commands must leave the old document and its snapshots together.
        // `mailings.finish` with a path saves the merged copy there and leaves the open
        // document alone, so it replaces nothing.
        let replaces_document = matches!(id, "review.compare" | "review.combine" | "mailings.envelopes" | "mailings.labels")
            || (id == "mailings.finish" && crate::p::str(params, "path").is_none());
        let versions_before = replaces_document.then(|| self.versions.clone());
        let identity_before = replaces_document.then(|| {
            (
                self.path.clone(),
                self.file_name.clone(),
                self.document_id,
                self.password.clone(),
                self.dirty,
                self.bib_style.clone(),
                self.merge.preview,
            )
        });
        let typing = spec.id == "text.insert" || spec.id == "equation.type";
        if spec.mutates && !typing {
            self.typing_open = false;
        }
        // Whether this command pushed an undo step (a no-op command's step is dropped below,
        // leaving its undo group without one).
        let mut pushed = false;
        if spec.mutates && spec.id != "view.markdownSourceText" {
            let label = if typing { "Typing" } else { spec.label };
            // A joined command merges into its group's step; a group whose first command
            // changed nothing has none, so the next command in it starts one (#4001).
            if !join || self.history.is_empty() || !self.join_anchor {
                pushed = self.checkpoint(label);
            }
        } else if !spec.id.starts_with("caret.") && !spec.id.starts_with("view.") {
            // Non-mutating commands other than caret movement keep the typing group.
        } else if spec.id.starts_with("caret.") {
            self.typing_open = false;
        }
        // Only the paste right after a tracked cut makes it a move.
        if (spec.mutates || matches!(id, "edit.undo" | "edit.undoTo" | "edit.redo")) && !matches!(id, "edit.cut" | "edit.paste" | "edit.pasteSpecial")
        {
            self.pending_move = None;
        }
        // bace: journal; the step this command will make (or join) is the one whose undo the log
        // can carry as a patch. `finish_success` decides whether this command may mark it.
        let journal_step = self.history.last().cloned();
        // bace: journal
        let journal_before = JournalBefore {
            sel: self.sel.clone(),
            pending: self.pending.clone(),
            rev: self.rev,
            document: self.document_id,
            blocked: journal_blocked,
        };
        let sel_before = self.sel.clone();
        // A rejected command must not persist a new proofing default, including a rejection
        // by the protected-region check after its handler has succeeded. Undo stays document-only.
        let default_language_before = self.prefs.default_language.clone();
        let run = spec.run;
        let mutates = spec.mutates;
        let ime_caret_before = self.current_ime_caret_state();
        let rev_before = self.rev;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match crate::cmd::canvas::member_command(self, id, params) {
            Some(r) => r,
            None => crate::cmd::column::dispatch(self, id, mutates, run, params),
        }));
        let mut result = match result {
            Ok(r) => r,
            Err(p) => {
                let msg = p.downcast_ref::<&str>().map(ToString::to_string).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
                Err(CmdError::Failed(format!("{id} panicked: {msg}")))
            }
        };
        if self.document_id != home || (result.is_err() && self.order.iter().any(|d| *d >= mark)) {
            return self.finish_elsewhere(
                ElsewhereContext { home, mark, before_doc, blocks_before, default_language_before, pending_application_change_before },
                result,
            );
        }
        // Restrict Editing: outside the exceptions nothing may change.
        if result.is_ok()
            && guard
            && let Some((before, ..)) = &before_doc
            && !crate::cmd::protect::outside_unchanged(before, &self.doc, &self.author)
        {
            result = Err(CmdError::Disabled(format!("{id}: only the editable regions of this protected document can change")));
        }
        match &result {
            Ok(_) => self.finish_success(SuccessContext {
                id,
                spec,
                before_doc: &before_doc,
                pending_before: &pending_before,
                document_id_before,
                replaces_document,
                dirty_before,
                rev_restore,
                pushed,
                journal_step,
                mutates,
                ime_caret_before,
                rev_before,
                sel_before: &sel_before,
                params,
                joins,
                journal_before: &journal_before,
            }),
            Err(e) => {
                self.ime_edit_outcome = ImeEditOutcome::default();
                self.pending_ime_application_change = pending_application_change_before;
                self.autocap = None;
                self.prefs.default_language = default_language_before;
                if let Some(epoch) = ime_epoch_before {
                    self.ime_body_epoch = epoch;
                }
                if let Some(math) = math_before {
                    self.math = math;
                }
                if let Some(versions) = versions_before {
                    self.versions = versions;
                }
                if let Some((path, file_name, document_id, password, dirty, bib_style, merge_preview)) = identity_before {
                    self.path = path;
                    self.file_name = file_name;
                    self.document_id = document_id;
                    self.password = password;
                    self.dirty = dirty;
                    self.bib_style = bib_style;
                    self.merge.preview = merge_preview;
                }
                if let Some((d, s, h, r, t, ev, c)) = before_doc {
                    self.doc = d;
                    self.sel = s;
                    self.history = h;
                    self.redo = r;
                    self.typing_open = t;
                    // The steps the command pushed out are back, so they no longer count as gone.
                    self.undo_evicted = ev;
                    self.column = c;
                    // Its step is gone with them: its undo group has none to merge into.
                    if pushed {
                        self.join_anchor = false;
                    }
                }
                if let Some(dirty) = dirty_before {
                    self.dirty = dirty;
                }
                if let Some(rev) = rev_restore {
                    self.restore_rev(rev);
                }
                if let Some(b) = blocks_before {
                    self.building_blocks = b;
                }
                self.status = e.to_string();
            }
        }
        result
    }

    fn finish_success(&mut self, context: SuccessContext<'_>) {
        let SuccessContext {
            id,
            spec,
            before_doc,
            pending_before,
            document_id_before,
            replaces_document,
            dirty_before,
            rev_restore,
            pushed,
            journal_step,
            mutates,
            ime_caret_before,
            rev_before,
            sel_before,
            params,
            joins,
            journal_before,
        } = context;
        let mut no_op = false;
        if spec.mutates {
            // Formatting changed while tracking becomes a formatting revision.
            if let Some((before, ..)) = before_doc {
                crate::cmd::fmt_revisions::record(self, id, before);
            }
            self.doc.ensure_nonempty();
            self.doc.prune_text_boxes();
            // Whether the command left the document, the selection and the formatting
            // waiting at the caret as they were. A command that replaces the document —
            // a mail merge, a comparison — is not an edit of this one, whatever the new
            // document holds: `replaces_document` names the commands that may replace
            // it, and `document_id` catches any other that does (through
            // `set_document`, which starts a new history the old one must not overwrite).
            let unchanged = |s: &Session| match (before_doc, pending_before) {
                (Some((doc, sel, _, _, _, _, column)), Some(pending)) => {
                    !replaces_document && s.document_id == document_id_before && s.unchanged(doc, sel, column, pending)
                }
                _ => false,
            };
            let changed = !unchanged(self);
            if changed {
                // The layout the connectors are rerouted against is the new one.
                self.touch();
            }
            // Glued connectors follow the shapes they join.
            let rerouted = crate::cmd::connectors::reroutes_after(id) && crate::cmd::connectors::reroute(self);
            if rerouted {
                self.touch();
            }
            // A command that changed nothing is not an edit (#23): it adds no undo
            // step, marks no unsaved changes, forces no relayout and leaves the redo
            // stack alone. What it left behind is compared with what it started from,
            // after the reroute: a reroute that moved a connector is an edit of its own.
            no_op = !changed && !rerouted && unchanged(self);
            if no_op {
                // The step the dispatch pushed is not an edit: put the stacks back as
                // they were, so the redo stack survives and no step is evicted. The
                // group it belonged to is left without a step of its own, so the next
                // command in it starts one.
                if let Some((_, _, history, redo, typing_open, evicted, _)) = before_doc {
                    self.history = history.clone();
                    self.redo = redo.clone();
                    self.typing_open = *typing_open;
                    self.undo_evicted = *evicted;
                }
                if let Some(dirty) = dirty_before {
                    self.dirty = dirty;
                }
                if let Some(rev) = rev_restore {
                    let touched = self.rev;
                    self.restore_rev(rev);
                    crate::cmd::draw::rekey_lasso(self, touched, rev);
                }
                if pushed {
                    self.join_anchor = false;
                }
            }
        }
        self.clamp_selection();
        let mut ime_outcome = self.ime_edit_outcome;
        if ime_outcome.content_changed {
            ime_outcome.state_changed = true;
        }
        // An unclassified mutating command is conservatively a state change. It can make
        // a frame Other, but it can never mint a receipt because `classified` is false.
        // A command that changed nothing is not an edit (#23), so it is not an IME
        // state change either: otherwise Backspace at the document start would still
        // signal an application change and move the settled IME revision.
        if !ime_outcome.classified && mutates && !no_op {
            ime_outcome.state_changed = true;
        }
        ime_outcome.selection_changed = self.current_ime_caret_state() != ime_caret_before;
        self.ime_edit_outcome = ime_outcome;
        if ime_outcome.state_changed {
            let caret = self.current_ime_caret_state();
            self.ime_state_seen = Some(caret);
            self.reset_ime_state_revision();
        }
        if !self.ime_origin && !no_op && (ime_outcome.content_changed || ime_outcome.state_changed || (!ime_outcome.classified && mutates)) {
            self.pending_ime_application_change = true;
        }
        if sel_before.focus.story == StoryRef::Body && self.in_header_footer() {
            self.hf_return = Some(sel_before.clone());
        }
        // A selection command that moves the insertion point ends the typing group, like
        // caret movement: typing, Select, typing are two undo steps (#419).
        if !spec.mutates && self.sel != *sel_before {
            self.typing_open = false;
        }
        if spec.mutates && !no_op && !matches!(id, "edit.undo" | "edit.redo") {
            self.note_edit();
        }
        // Outline view: collapsed headings follow edits; the caret stays where it shows.
        crate::cmd::outline::after_command(self, sel_before, self.rev != rev_before);
        // Extra selected objects last until something else is edited or selected; formatting
        // them keeps them selected, as in Word.
        if !matches!(id, "select.addObject" | "arrange.ungroup" | "shape.fill" | "shape.effects") && (spec.mutates || self.sel != *sel_before) {
            self.also_selected.clear();
        }
        // A canvas member stays picked while its canvas does and nothing else is selected.
        if id != "select.member"
            && (self.sel != *sel_before
                || id.starts_with("select.")
                || id.starts_with("caret.")
                || matches!(id, "edit.undo" | "edit.undoTo" | "edit.redo"))
        {
            self.member = None;
        }
        self.note_typing(id, before_doc.as_ref().map(|b| &b.0), sel_before);
        self.note_journal(id, params, spec.journal, joins, journal_before); // bace: journal
        // bace: journal; whether Undo can send this step as patches. A command that joins an
        // older step may only keep the mark both commands earned.
        if !no_op
            && spec.mutates
            && spec.id != "view.markdownSourceText"
            && let Some(step) = journal_step
            && let Some(at) = self.history.iter().position(|history| Arc::ptr_eq(history, &step))
        {
            let journaled = spec.journal && !journal_before.blocked && self.document_id == journal_before.document;
            let step = Arc::make_mut(&mut self.history[at]);
            step.journaled = if pushed { journaled } else { step.journaled && journaled };
        }
    }

    /// The end of [`Session::run`] for a command that ended up in another document than it
    /// started in (it opened, made, switched to or closed one) or that failed after opening one.
    /// The checks that compare the document before and after the command don't apply.
    /// Succeeded: the first document loses the undo step made for the command when its text is
    /// as it was (opening a document is no edit to undo there). Failed: the documents the command opened close again and the first document is
    /// active and as it was.
    fn finish_elsewhere(&mut self, context: ElsewhereContext, result: Result<Value, CmdError>) -> Result<Value, CmdError> {
        let ElsewhereContext { home, mark, before_doc, blocks_before, default_language_before, pending_application_change_before } = context;
        // Another document is a state change the input method has to see, never an IME edit.
        self.ime_edit_outcome = ImeEditOutcome::default();
        match &result {
            Ok(_) => {
                self.put_back_parked(home, before_doc);
                // bace: journal; a command that opened, made, switched to or closed a document
                // is not an edit of the one the log describes: the host offers a new base.
                if !self.applying {
                    self.journal.cut();
                }
                self.clamp_selection();
                self.ime_state_seen = Some(self.current_ime_caret_state());
                self.reset_ime_state_revision();
                if !self.ime_origin {
                    self.pending_ime_application_change = true;
                }
            }
            Err(e) => {
                self.pending_ime_application_change = pending_application_change_before;
                self.autocap = None;
                self.prefs.default_language = default_language_before;
                if self.is_open(home) {
                    let _ = self.switch_to(home);
                    let opened: Vec<u64> = self.order.iter().copied().filter(|d| *d >= mark && *d != home).collect();
                    for d in opened {
                        let _ = self.close_document(d);
                    }
                    if let Some((d, s, h, r, t, ev, c)) = before_doc {
                        self.doc = d;
                        self.sel = s;
                        self.history = h;
                        self.redo = r;
                        self.typing_open = t;
                        self.undo_evicted = ev;
                        self.column = c;
                    }
                }
                if let Some(b) = blocks_before {
                    self.building_blocks = b;
                }
                self.status = e.to_string();
            }
        }
        result
    }

    // bace: the journal API a host mirrors the document with.
    /// What the session journaled since the last call, for a host that sends it to another session
    /// (`crate::journal`). A cut means the log lost its beginning: the host has to offer the
    /// document as the new base instead.
    pub fn take_journal(&mut self) -> crate::journal::Taken {
        self.journal.take()
    }

    /// The steps are inside a document the host is offering as the log's next base: forget them
    /// rather than sending them twice.
    pub fn skip_journal(&mut self) {
        self.journal.clear();
    }

    /// How many entries the journal holds (one per typing run or drag), for traces and tests.
    pub fn journal_entries(&self) -> usize {
        self.journal.len()
    }

    /// Runs the steps another session journaled (`crate::journal`), in order.
    ///
    /// Each is a command, preceded by a `journal.state` step with the selection and the formatting
    /// waiting at the caret that session had, so the command edits the same place. The person's
    /// own caret, pending formatting and extra selection are put back afterwards, and nothing run
    /// here is journaled in turn: two sessions can send each other's steps without echoing them.
    /// Returns how many commands ran (one the document refuses is skipped, as a step from another
    /// version would be).
    pub fn apply_journal(&mut self, steps: &[(String, Value)]) -> usize {
        let own = self.own_state();
        self.applying = true;
        let mut applied = 0;
        for (id, params) in steps {
            if id == crate::journal::STATE {
                self.apply_journal_state(params);
                continue;
            }
            // bace: journal; a difference, not a command to run.
            if id == crate::journal::REVERT {
                if self.apply_journal_revert(params) {
                    applied += 1;
                }
                continue;
            }
            match self.run(id, params) {
                Ok(_) => applied += 1,
                Err(error) => log::warn!("a journaled {id} was not applied: {error}"),
            }
        }
        self.applying = false;
        self.restore_own_state(own);
        applied
    }

    // bace: journal; applies the block patches a remote Undo or Redo sent.
    /// Applies the block patches an Undo or Redo sent (`crate::journal::REVERT`).
    ///
    /// A patch names a story, a container path, an index and the blocks that replace a range
    /// there. The session that sent it changed the same blocks, so applying them makes the two
    /// documents agree without making either one a new base. The person's caret is put back by
    /// [`Self::restore_own_state`] afterwards, like any other replayed step.
    fn apply_journal_revert(&mut self, params: &Value) -> bool {
        let Some(patches) = params.get("patches").and_then(Value::as_array) else { return false };
        let doc = self.doc.clone();
        let sel = self.sel.clone();
        let pending = self.pending.clone();
        let column = self.column.clone();
        let mut changed = false;
        for patch in patches {
            let (Some(story), Some(path), Some(at), Some(remove), Some(blocks)) = (
                patch.get("story").and_then(|story| serde_json::from_value::<StoryRef>(story.clone()).ok()),
                patch
                    .get("path")
                    .and_then(Value::as_array)
                    .map(|path| path.iter().filter_map(Value::as_u64).map(|index| index as u32).collect::<Vec<_>>()),
                patch.get("at").and_then(Value::as_u64).map(|at| at as usize),
                patch.get("remove").and_then(Value::as_u64).map(|remove| remove as usize),
                patch.get("blocks").and_then(|blocks| serde_json::from_value::<Vec<Arc<Block>>>(blocks.clone()).ok()),
            ) else {
                continue;
            };
            let mut container = path;
            // `container_mut` reads the parent of the path, so any index works as the last one.
            container.push(0);
            let Ok(blocks_at) = self.doc.container_mut(story, &Path(container)) else { continue };
            let Some(end) = at.checked_add(remove) else { continue };
            if end > blocks_at.len() {
                continue;
            }
            blocks_at.splice(at..end, blocks);
            changed = true;
        }
        if changed {
            self.doc.ensure_nonempty();
            // The remote change is one step in this session's history too, so its Undo takes the
            // remote Undo back (and sends the inverse as another patch). Without this, the
            // person's ⌘Z would step over a change their document shows.
            self.history.push(Arc::new(Undo { label: "Undo".to_string(), doc, sel, pending, column, journaled: true }));
            if self.history.len() > MAX_UNDO {
                self.history.remove(0);
                self.undo_evicted += 1;
            }
            self.redo.clear();
            self.typing_open = false;
            self.join_anchor = true;
            self.note_body_replaced();
            self.touch();
        }
        changed
    }

    /// The state the person here is editing with, which replaying another session's steps must
    /// leave as it found it.
    fn own_state(&self) -> OwnState {
        OwnState {
            sel: self.sel.clone(),
            pending: self.pending.clone(),
            also_selected: self.also_selected.clone(),
            goal_x: self.goal_x,
            typing_open: self.typing_open,
            join_next: self.join_next,
            edit_marks: self.edit_marks.clone(),
            go_back_step: self.go_back_step,
        }
    }

    /// Puts [`Session::own_state`] back.
    fn restore_own_state(&mut self, own: OwnState) {
        let OwnState { sel, pending, also_selected, goal_x, typing_open, join_next, edit_marks, go_back_step } = own;
        self.sel = sel;
        self.pending = pending;
        self.also_selected = also_selected;
        self.goal_x = goal_x;
        self.typing_open = typing_open;
        self.join_next = join_next;
        self.edit_marks = edit_marks;
        self.go_back_step = go_back_step;
        self.clamp_selection();
    }

    /// Puts the selection and the formatting waiting at the caret back where the session that
    /// journaled the next step had them (`crate::journal`).
    ///
    /// A column block, a whole-cell selection or an equation being edited here ends: the selection
    /// is the plain one the other session had, so a block this document no longer matches cannot
    /// make the step edit something else.
    fn apply_journal_state(&mut self, params: &Value) {
        if let (Some(anchor), Some(focus)) =
            (params.get("anchor").and_then(crate::cmd::parse_pos), params.get("focus").and_then(crate::cmd::parse_pos))
        {
            self.sel = Selection { anchor: self.doc.clamp(&anchor), focus: self.doc.clamp(&focus) };
            self.goal_x = None;
        }
        self.pending = match params.get("pending") {
            Some(Value::Null) | None => None,
            Some(props) => serde_json::from_value(props.clone()).ok(),
        };
        self.column = None;
        self.widened = None;
        self.math = None;
    }

    /// Records the command that just ran for the host's journal, or cuts the log when a step cannot
    /// describe it (`crate::journal`): the command is not marked `journal`, or it read a column
    /// block, whole table cells, an equation or extra selected objects.
    fn note_journal(&mut self, id: &str, params: &Value, journaled: bool, joins: crate::journal::Joins, before: &JournalBefore) {
        // A step another session sent is replayed here: it is not journaled in turn.
        if self.applying {
            return;
        }
        // bace: journal; Undo, Undo To and Redo record their own difference
        // (`Session::undo` / `redo`, `crate::journal::record_revert`), or cut the log when it is
        // not one they can carry. Repeat re-runs its command, which is recorded below; recording
        // the wrapper too would duplicate that command.
        if matches!(id, "edit.undo" | "edit.undoTo" | "edit.redo" | "edit.repeat") {
            return;
        }
        // A command that changed nothing (a read, a view toggle, a dialog) is not an edit.
        let replaced = self.document_id != before.document;
        if !replaced && self.rev == before.rev {
            return;
        }
        // A replaced document (open, new, mail merge, recover) is not an edit of the one the log
        // describes: the host offers it as the log's next base.
        if replaced || !journaled || before.blocked {
            self.journal.cut();
            return;
        }
        self.journal.record(id, params, &before.sel, before.pending.as_ref(), joins);
    }
}
