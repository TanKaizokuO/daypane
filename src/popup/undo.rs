//! In-memory undo stack. Each entry stores the *inverse* of the action the user performed,
//! so undoing is just applying it: complete↔reopen, delete↔restore, edit→previous values,
//! create→delete.

use anyhow::Result;

use crate::db::{Db, Task, TaskPatch};

const MAX_UNDO: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Complete(i64),
    Reopen(i64),
    Delete(i64),
    Restore(i64),
    Patch(i64, TaskPatch),
}

impl Action {
    pub fn task_id(&self) -> i64 {
        match self {
            Self::Complete(id)
            | Self::Reopen(id)
            | Self::Delete(id)
            | Self::Restore(id)
            | Self::Patch(id, _) => *id,
        }
    }

    /// The action that reverses `self`. `before` is the task as it was before `self` is applied
    /// (only needed to restore edited fields).
    pub fn inverse(&self, before: &Task) -> Action {
        match self {
            Self::Complete(id) => Self::Reopen(*id),
            Self::Reopen(id) => Self::Complete(*id),
            Self::Delete(id) => Self::Restore(*id),
            Self::Restore(id) => Self::Delete(*id),
            Self::Patch(id, p) => Self::Patch(*id, inverse_patch(before, p)),
        }
    }

    pub fn apply(&self, db: &Db) -> Result<Task> {
        match self {
            Self::Complete(id) => db.complete_task(*id),
            Self::Reopen(id) => db.reopen_task(*id),
            Self::Delete(id) => db.delete_task(*id),
            Self::Restore(id) => db.restore_task(*id),
            Self::Patch(id, p) => db.update_task(*id, p.clone()),
        }
    }
}

/// A patch restoring exactly the fields `p` changes, to the values in `before`.
pub fn inverse_patch(before: &Task, p: &TaskPatch) -> TaskPatch {
    let mut inv = TaskPatch::default();
    if p.title.is_some() {
        inv.title = Some(before.title.clone());
    }
    if p.due_date.is_some() {
        inv.due_date = Some(before.due_date);
        // Changing the date can implicitly clear the time (date cleared) — restore it too.
        if p.due_time.is_none() && before.due_time.is_some() {
            inv.due_time = Some(before.due_time);
        }
    }
    if p.due_time.is_some() {
        inv.due_time = Some(before.due_time);
    }
    if p.notes.is_some() {
        inv.notes = Some(before.notes.clone());
    }
    if p.priority.is_some() {
        inv.priority = Some(before.priority);
    }
    if p.tags.is_some() {
        inv.tags = Some(before.tags.clone());
    }
    if p.links.is_some() {
        inv.links = Some(before.links.clone());
    }
    inv
}

#[derive(Debug, Clone, PartialEq)]
pub struct UndoEntry {
    /// What the user did, for the status line (e.g. `complete “Pay rent”`).
    pub label: String,
    pub action: Action,
}

#[derive(Debug, Default)]
pub struct UndoStack {
    entries: Vec<UndoEntry>,
}

impl UndoStack {
    pub fn push(&mut self, label: String, inverse: Action) {
        if self.entries.len() == MAX_UNDO {
            self.entries.remove(0);
        }
        self.entries.push(UndoEntry {
            label,
            action: inverse,
        });
    }

    pub fn pop(&mut self) -> Option<UndoEntry> {
        self.entries.pop()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveTime};

    use super::*;
    use crate::db::{NewTask, Priority};

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn timed(db: &Db) -> Task {
        db.create_task(NewTask {
            title: "Pay rent".into(),
            due_date: Some(d("2026-11-14")),
            due_time: NaiveTime::from_hms_opt(17, 0, 0),
            notes: "landlord".into(),
            priority: Priority::Low,
            tags: vec!["home".into()],
            ..Default::default()
        })
        .unwrap()
    }

    /// Apply `a`, then its inverse; the task must match the original in every user-visible field.
    fn round_trip(db: &Db, a: Action) -> (Task, Task) {
        let before = db.get_task(a.task_id()).unwrap().unwrap();
        let inv = a.inverse(&before);
        a.apply(db).unwrap();
        inv.apply(db).unwrap();
        (before, db.get_task(a.task_id()).unwrap().unwrap())
    }

    fn same(a: &Task, b: &Task) {
        assert_eq!(
            (
                &a.title,
                a.due_date,
                a.due_time,
                &a.notes,
                a.priority,
                &a.tags,
                &a.links,
                a.completed_at,
                a.deleted_at
            ),
            (
                &b.title,
                b.due_date,
                b.due_time,
                &b.notes,
                b.priority,
                &b.tags,
                &b.links,
                b.completed_at,
                b.deleted_at
            )
        );
    }

    #[test]
    fn complete_and_delete_invert() {
        let db = Db::open_in_memory().unwrap();
        let t = timed(&db);
        for a in [Action::Complete(t.id), Action::Delete(t.id)] {
            let (before, after) = round_trip(&db, a);
            same(&before, &after);
        }
        // And the other direction, starting from a completed / trashed task.
        db.complete_task(t.id).unwrap();
        db.delete_task(t.id).unwrap();
        let (before, after) = round_trip(&db, Action::Reopen(t.id));
        same(&before, &after);
        let (before, after) = round_trip(&db, Action::Restore(t.id));
        same(&before, &after);
    }

    #[test]
    fn edit_inverse_restores_changed_fields_only() {
        let db = Db::open_in_memory().unwrap();
        let t = timed(&db);
        let patch = TaskPatch {
            title: Some("Pay rent now".into()),
            priority: Some(Priority::High),
            tags: Some(vec!["urgent".into()]),
            ..Default::default()
        };
        let inv = inverse_patch(&t, &patch);
        assert_eq!(inv.title.as_deref(), Some("Pay rent"));
        assert_eq!(inv.priority, Some(Priority::Low));
        assert_eq!(inv.notes, None, "untouched fields stay untouched");
        let (before, after) = round_trip(&db, Action::Patch(t.id, patch));
        same(&before, &after);
    }

    #[test]
    fn edit_inverse_restores_time_cleared_with_date() {
        let db = Db::open_in_memory().unwrap();
        let t = timed(&db);
        let patch = TaskPatch {
            due_date: Some(None),
            ..Default::default()
        };
        let (before, after) = round_trip(&db, Action::Patch(t.id, patch));
        same(&before, &after);
        assert_eq!(after.due_time, NaiveTime::from_hms_opt(17, 0, 0));
    }

    #[test]
    fn edit_inverse_restores_date_and_time_set_together() {
        let db = Db::open_in_memory().unwrap();
        let t = db
            .create_task(NewTask {
                title: "x".into(),
                ..Default::default()
            })
            .unwrap();
        let patch = TaskPatch {
            due_date: Some(Some(d("2026-11-20"))),
            due_time: Some(NaiveTime::from_hms_opt(8, 30, 0)),
            ..Default::default()
        };
        let (before, after) = round_trip(&db, Action::Patch(t.id, patch));
        same(&before, &after);
    }

    #[test]
    fn create_is_undone_by_delete() {
        let db = Db::open_in_memory().unwrap();
        let t = timed(&db);
        let mut stack = UndoStack::default();
        stack.push("add “Pay rent”".into(), Action::Delete(t.id));
        let entry = stack.pop().unwrap();
        entry.action.apply(&db).unwrap();
        assert!(db.get_task(t.id).unwrap().unwrap().deleted_at.is_some());
        assert!(stack.pop().is_none());
    }

    #[test]
    fn stack_is_bounded() {
        let mut stack = UndoStack::default();
        for i in 0..(MAX_UNDO as i64 + 10) {
            stack.push(String::new(), Action::Delete(i));
        }
        assert_eq!(stack.len(), MAX_UNDO);
        assert_eq!(
            stack.pop().unwrap().action,
            Action::Delete(MAX_UNDO as i64 + 9)
        );
    }
}
