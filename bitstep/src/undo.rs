//! The edit history behind undo and redo.
//!
//! Pure bookkeeping over whole-sequence states: [`crate::model::SeqModel`]
//! owns one [`History`] of [`crate::state::SeqSnapshot`]s and decides when to
//! call it; this module only keeps the stacks straight, so it is tested here
//! with plain numbers.
//!
//! The scheme is "commit what changed": the model counts edits, and the
//! editor calls [`History::commit`] whenever the user is between gestures (no
//! button down, no text field being typed in). If the count moved since the
//! last commit, the state as it was *then* becomes one undo step. So a paint
//! drag, a length drag or a typed song order is one step however many writes
//! it made, and no writer has to know undo exists beyond bumping the count.
//!
//! Like `model`, `sequencer`, `voices` and `state`, this module imports no
//! nice-plug types.

use std::collections::VecDeque;

/// How many undo steps are kept. The oldest is dropped past this.
///
/// A step is a whole-sequence snapshot, which leaves out untouched patterns:
/// a few kilobytes for a typical drum patch, so a hundred of them cost less
/// than the editor's fonts.
pub const UNDO_DEPTH: usize = 100;

/// The undo and redo stacks, plus the state as of the last commit.
#[derive(Debug)]
pub struct History<T> {
    /// Oldest first, so the depth cap drops from the front.
    undo: VecDeque<T>,
    /// Most recent undo last.
    redo: Vec<T>,
    /// The state as of the last commit and the edit count it was taken at;
    /// `None` until the first commit, and again after [`History::clear`].
    baseline: Option<(u64, T)>,
}

impl<T: Clone + PartialEq> History<T> {
    pub fn new() -> History<T> {
        History { undo: VecDeque::new(), redo: Vec::new(), baseline: None }
    }

    /// Records whatever changed since the last commit as one undo step.
    ///
    /// `edits` is the model's edit count; `current` reads the state now and
    /// is only called when the count moved (or on the first commit), because
    /// it allocates a whole snapshot. A count that moved without the state
    /// changing — a write of the value already there — records nothing.
    /// A new step clears the redo stack, as in every editor.
    pub fn commit(&mut self, edits: u64, current: impl FnOnce() -> T) {
        if self.baseline.as_ref().is_some_and(|(seen, _)| *seen == edits) {
            return;
        }
        let now = current();
        if let Some((_, before)) = self.baseline.take()
            && before != now
        {
            push_capped(&mut self.undo, before);
            self.redo.clear();
        }
        self.baseline = Some((edits, now));
    }

    /// One step back: the state to restore, or `None` if there is nothing to
    /// undo. Commit first, so the last edit is on the stack.
    ///
    /// The caller restores the returned state and then calls
    /// [`History::rebase`] with the edit count the restore left behind.
    pub fn undo(&mut self) -> Option<T> {
        let (seen, now) = self.baseline.take()?;
        let Some(before) = self.undo.pop_back() else {
            self.baseline = Some((seen, now));
            return None;
        };
        self.redo.push(now);
        self.baseline = Some((seen, before.clone()));
        Some(before)
    }

    /// One step forward again: the mirror of [`History::undo`].
    pub fn redo(&mut self) -> Option<T> {
        let (seen, now) = self.baseline.take()?;
        let Some(after) = self.redo.pop() else {
            self.baseline = Some((seen, now));
            return None;
        };
        push_capped(&mut self.undo, now);
        self.baseline = Some((seen, after.clone()));
        Some(after)
    }

    /// The state [`History::undo`] or [`History::redo`] returned has been
    /// restored, and the model's edit count is now `edits`: the restore
    /// itself is not an edit to record.
    pub fn rebase(&mut self, edits: u64) {
        if let Some((seen, _)) = self.baseline.as_mut() {
            *seen = edits;
        }
    }

    /// Forgets everything — the host loaded a different sequence, and undoing
    /// into the one it replaced would be a surprise. The next commit takes a
    /// fresh baseline.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.baseline = None;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

impl<T: Clone + PartialEq> Default for History<T> {
    fn default() -> History<T> {
        History::new()
    }
}

fn push_capped<T>(stack: &mut VecDeque<T>, state: T) {
    if stack.len() >= UNDO_DEPTH {
        stack.pop_front();
    }
    stack.push_back(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in model: a value and an edit count, edited and restored the
    /// way `SeqModel` does it.
    struct Fake {
        value: i32,
        edits: u64,
        history: History<i32>,
    }

    impl Fake {
        fn new(value: i32) -> Fake {
            let mut fake = Fake { value, edits: 0, history: History::new() };
            fake.commit();
            fake
        }

        fn set(&mut self, value: i32) {
            self.value = value;
            self.edits += 1;
        }

        fn commit(&mut self) {
            let value = self.value;
            self.history.commit(self.edits, || value);
        }

        fn undo(&mut self) -> bool {
            self.commit();
            match self.history.undo() {
                Some(value) => {
                    self.set(value);
                    self.history.rebase(self.edits);
                    true
                }
                None => false,
            }
        }

        fn redo(&mut self) -> bool {
            self.commit();
            match self.history.redo() {
                Some(value) => {
                    self.set(value);
                    self.history.rebase(self.edits);
                    true
                }
                None => false,
            }
        }
    }

    #[test]
    fn edits_between_two_commits_are_one_step() {
        let mut fake = Fake::new(0);
        fake.set(1);
        fake.set(2);
        fake.set(3);
        fake.commit();
        assert!(fake.undo());
        assert_eq!(fake.value, 0, "the whole gesture came back as one");
        assert!(!fake.undo(), "and there was nothing before it");
        assert_eq!(fake.value, 0);
    }

    #[test]
    fn undo_and_redo_walk_the_steps_both_ways() {
        let mut fake = Fake::new(0);
        for value in 1..=3 {
            fake.set(value);
            fake.commit();
        }
        assert!(fake.undo());
        assert!(fake.undo());
        assert_eq!(fake.value, 1);
        assert!(fake.redo());
        assert_eq!(fake.value, 2);
        assert!(fake.redo());
        assert_eq!(fake.value, 3);
        assert!(!fake.redo(), "nothing left to redo");
        assert_eq!(fake.value, 3);
    }

    #[test]
    fn an_uncommitted_edit_is_committed_by_undo() {
        let mut fake = Fake::new(0);
        fake.set(5);
        assert!(fake.undo(), "the edit still in progress is the step undone");
        assert_eq!(fake.value, 0);
        assert!(fake.redo());
        assert_eq!(fake.value, 5);
    }

    #[test]
    fn a_new_edit_after_undo_clears_redo() {
        let mut fake = Fake::new(0);
        fake.set(1);
        fake.commit();
        assert!(fake.undo());
        fake.set(7);
        fake.commit();
        assert!(!fake.history.can_redo());
        assert!(!fake.redo());
        assert_eq!(fake.value, 7);
        assert!(fake.undo());
        assert_eq!(fake.value, 0);
    }

    #[test]
    fn a_restore_is_not_recorded_as_an_edit() {
        let mut fake = Fake::new(0);
        fake.set(1);
        fake.commit();
        assert!(fake.undo());
        fake.commit();
        fake.commit();
        assert!(fake.history.can_redo(), "committing after an undo keeps the redo");
        assert!(!fake.history.can_undo());
    }

    #[test]
    fn a_write_that_changed_nothing_records_nothing() {
        let mut fake = Fake::new(4);
        fake.set(4);
        fake.commit();
        assert!(!fake.history.can_undo());
        assert!(!fake.undo());
    }

    #[test]
    fn the_depth_cap_drops_the_oldest_step() {
        let mut fake = Fake::new(0);
        for value in 1..=(UNDO_DEPTH as i32 + 10) {
            fake.set(value);
            fake.commit();
        }
        let mut undone = 0;
        while fake.undo() {
            undone += 1;
        }
        assert_eq!(undone, UNDO_DEPTH);
        assert_eq!(fake.value, 10, "the ten oldest steps were dropped");
    }

    #[test]
    fn clear_forgets_both_stacks_and_rebaselines() {
        let mut fake = Fake::new(0);
        fake.set(1);
        fake.commit();
        fake.set(2);
        fake.commit();
        assert!(fake.undo());
        fake.history.clear();
        assert!(!fake.history.can_undo() && !fake.history.can_redo());

        // A load lands: the next commit only takes a baseline.
        fake.set(9);
        fake.commit();
        assert!(!fake.undo());
        assert_eq!(fake.value, 9);
        fake.set(10);
        fake.commit();
        assert!(fake.undo());
        assert_eq!(fake.value, 9);
    }

    #[test]
    fn undo_before_any_commit_does_nothing() {
        let mut history = History::<i32>::new();
        assert_eq!(history.undo(), None);
        assert_eq!(history.redo(), None);
    }
}
