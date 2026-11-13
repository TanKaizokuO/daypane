//! Rendering: month grid, event list, task list. Every render rebuilds from the database.

use std::rc::Rc;

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate};
use gtk::prelude::*;
use gtk::{glib, graphene};
use gtk4 as gtk;

use super::format;
use super::layout;
use super::ui::{set_class, Ui};
use crate::db::{self, Event, Status, Task, TaskFilter};

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn text_label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(classes.to_vec())
        .build()
}

fn empty_label(text: &str) -> gtk::Label {
    text_label(text, &["empty"])
}

impl Ui {
    /// Re-render both panes from the database, keeping the selection.
    pub fn refresh(self: &Rc<Self>) {
        self.state.borrow_mut().today = Local::now().date_naive();
        let result = self
            .render_calendar()
            .and_then(|()| self.render_events())
            .and_then(|()| self.render_tasks());
        if let Err(e) = result {
            self.set_status(&format!("Database error: {e:#}"), true);
        }
    }

    pub fn rerender_tasks(self: &Rc<Self>) {
        if let Err(e) = self.render_tasks() {
            self.set_status(&format!("Database error: {e:#}"), true);
        }
    }

    // ───────────────────────────── left pane ─────────────────────────────

    pub fn render_calendar(&self) -> Result<()> {
        let (month, selected, today) = {
            let s = self.state.borrow();
            (s.month, s.selected, s.today)
        };
        self.w.month_label.set_text(&layout::month_label(month));
        let days = layout::grid_days(month);
        let from = db::local_day_bounds(days[0]).0;
        let to = db::local_day_bounds(days[days.len() - 1]).1;
        let events = self.db.events_between(from, to)?;
        let tasks = self.db.list_tasks(&TaskFilter {
            status: Status::Open,
            ..Default::default()
        })?;
        let marks = layout::day_marks(&events, &tasks, &Local);

        for (cell, day) in self.w.cells.iter().zip(&days) {
            cell.date.set(*day);
            cell.number.set_text(&day.day().to_string());
            set_class(&cell.button, "outside-month", day.month() != month.month());
            set_class(&cell.button, "today", *day == today);
            set_class(&cell.button, "selected-day", Some(*day) == selected);
            set_class(&cell.button, "has-events", marks.events.contains(day));
            set_class(&cell.button, "has-tasks", marks.tasks.contains(day));
        }
        Ok(())
    }

    pub fn render_events(&self) -> Result<()> {
        let (selected, today, search) = {
            let s = self.state.borrow();
            (s.selected, s.today, s.search.clone())
        };
        clear(&self.w.events_box);
        let (events, with_date, empty_text) = match search.as_deref() {
            Some(q) if q.trim().is_empty() => {
                self.w.events_title.set_text("Events");
                (Vec::new(), true, "Type to search events")
            }
            Some(q) => {
                self.w
                    .events_title
                    .set_text(&format!("Events matching “{}”", q.trim()));
                (self.db.search_events(q)?, true, "No matching events")
            }
            None => {
                let day = selected.unwrap_or(today);
                self.w.events_title.set_text(&format::day_title(day, today));
                (self.db.events_on(day)?, false, "No events")
            }
        };
        if events.is_empty() {
            self.w.events_box.append(&empty_label(empty_text));
        }
        for e in &events {
            self.w.events_box.append(&self.event_row(e, with_date));
        }
        Ok(())
    }

    fn event_row(&self, e: &Event, with_date: bool) -> gtk::Box {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["event"])
            .build();
        let head = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        head.append(&text_label(
            &format::event_when(e, &Local, with_date),
            &["event-time"],
        ));
        let summary = text_label(
            if e.summary.is_empty() {
                "(no title)"
            } else {
                &e.summary
            },
            &["event-summary"],
        );
        summary.set_hexpand(true);
        summary.set_wrap(true);
        summary.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        head.append(&summary);
        row.append(&head);
        if let Some(loc) = e.location.as_deref().filter(|l| !l.trim().is_empty()) {
            let l = text_label(loc, &["event-location"]);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            row.append(&l);
        }
        if let Some(meet) = e.meet_link.as_deref().filter(|l| !l.is_empty()) {
            let link = gtk::LinkButton::with_label(meet, "Join Meet");
            link.set_halign(gtk::Align::Start);
            link.add_css_class("event-meet");
            row.append(&link);
        }
        row
    }

    // ───────────────────────────── right pane ─────────────────────────────

    pub fn render_tasks(self: &Rc<Self>) -> Result<()> {
        let (today, selected, search, inbox_open, prev_sel, prev_index) = {
            let s = self.state.borrow();
            let idx = s
                .task_sel
                .and_then(|id| s.nav.iter().position(|&i| i == id));
            (
                s.today,
                s.selected,
                s.search.clone(),
                s.inbox_open,
                s.task_sel,
                idx,
            )
        };
        let scroll_value = self.w.task_scroll.vadjustment().value();
        clear(&self.w.task_box);
        self.rows.borrow_mut().clear();
        let mut nav: Vec<i64> = Vec::new();
        let list = &self.w.task_box;

        match (search.as_deref(), selected) {
            (Some(q), _) => {
                self.w.tasks_title.set_text("Search results");
                if q.trim().is_empty() {
                    list.append(&empty_label("Type to search tasks and events"));
                } else {
                    let tasks = self.db.list_tasks(&TaskFilter {
                        status: Status::All,
                        query: Some(q.to_string()),
                        ..Default::default()
                    })?;
                    if tasks.is_empty() {
                        list.append(&empty_label("No matching tasks"));
                    }
                    self.add_rows(list, &tasks, today, true, &mut nav);
                }
            }
            (None, Some(day)) => {
                self.w.tasks_title.set_text(&format::day_title(day, today));
                let mut tasks = self.db.list_tasks(&TaskFilter {
                    status: Status::All,
                    date: Some(day),
                    ..Default::default()
                })?;
                // Open tasks first; completed ones stay visible (and re-openable) below them.
                tasks.sort_by_key(|t| t.completed_at.is_some());
                if tasks.is_empty() {
                    list.append(&empty_label("No tasks on this day"));
                }
                self.add_rows(list, &tasks, today, false, &mut nav);
            }
            (None, None) => {
                self.w.tasks_title.set_text("Today");
                let view = self.db.today_view(today)?;
                if !view.overdue.is_empty() {
                    list.append(&text_label("Overdue", &["section-header", "overdue"]));
                    self.add_rows(list, &view.overdue, today, true, &mut nav);
                }
                list.append(&text_label("Due today", &["section-header", "due-today"]));
                if view.due_today.is_empty() {
                    list.append(&empty_label("Nothing due today"));
                }
                self.add_rows(list, &view.due_today, today, false, &mut nav);

                let inbox_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(2)
                    .build();
                let expander = gtk::Expander::builder()
                    .label(format!("Inbox ({})", view.inbox.len()))
                    .expanded(inbox_open)
                    .focusable(false)
                    .css_classes(["section-header", "inbox"])
                    .child(&inbox_box)
                    .build();
                if view.inbox.is_empty() {
                    inbox_box.append(&empty_label("No undated tasks"));
                }
                if inbox_open {
                    self.add_rows(&inbox_box, &view.inbox, today, false, &mut nav);
                } else {
                    // Collapsed rows are not keyboard-reachable, but still get built so expanding is instant.
                    let mut hidden = Vec::new();
                    self.add_rows(&inbox_box, &view.inbox, today, false, &mut hidden);
                    self.rows
                        .borrow_mut()
                        .retain(|(id, _)| !hidden.contains(id));
                }
                let ui = Rc::downgrade(self);
                expander.connect_expanded_notify(move |e| {
                    if let Some(ui) = ui.upgrade() {
                        ui.state.borrow_mut().inbox_open = e.is_expanded();
                        // Rebuilding inside the signal that is toggling the expander is unsafe; defer.
                        glib::idle_add_local_once(move || ui.rerender_tasks());
                    }
                });
                list.append(&expander);
            }
        }

        {
            let mut s = self.state.borrow_mut();
            s.task_sel = layout::reselect(&nav, prev_sel, prev_index);
            s.nav = nav;
        }
        self.apply_selection();
        let ui = self.clone();
        glib::idle_add_local_once(move || {
            ui.w.task_scroll.vadjustment().set_value(scroll_value);
            ui.scroll_to_selected_now();
        });
        Ok(())
    }

    fn add_rows(
        self: &Rc<Self>,
        container: &gtk::Box,
        tasks: &[Task],
        today: NaiveDate,
        show_date: bool,
        nav: &mut Vec<i64>,
    ) {
        for t in tasks {
            let row = self.task_row(t, today, show_date);
            container.append(&row);
            self.rows.borrow_mut().push((t.id, row.upcast()));
            nav.push(t.id);
        }
    }

    fn task_row(self: &Rc<Self>, t: &Task, today: NaiveDate, show_date: bool) -> gtk::Box {
        let done = t.completed_at.is_some();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["task-row"])
            .build();
        set_class(&row, "completed", done);
        set_class(&row, "overdue", t.is_overdue(today));
        set_class(&row, "today", t.due_date == Some(today));
        if let Some(c) = format::priority_class(t.priority) {
            row.add_css_class(c);
        }

        let id = t.id;
        let check = gtk::CheckButton::builder()
            .active(done)
            .focusable(false)
            .valign(gtk::Align::Start)
            .css_classes(["task-check"])
            .build();
        let ui = Rc::downgrade(self);
        check.connect_toggled(move |_| {
            if let Some(ui) = ui.upgrade() {
                // The row is rebuilt by the action; do it outside this signal emission.
                glib::idle_add_local_once(move || {
                    ui.click_task(id);
                    ui.toggle_complete(id);
                });
            }
        });
        row.append(&check);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        let head = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        let title = text_label(&t.title, &["task-title"]);
        title.set_hexpand(true);
        title.set_wrap(true);
        title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        head.append(&title);
        if let Some(when) = format::task_when(t, show_date) {
            head.append(&text_label(&when, &["task-time"]));
        }
        if let (Some(label), Some(class)) = (
            format::priority_label(t.priority),
            format::priority_class(t.priority),
        ) {
            head.append(&text_label(label, &["task-priority", class]));
        }
        body.append(&head);

        if !t.tags.is_empty() {
            let tags = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .build();
            for tag in &t.tags {
                tags.append(&text_label(&format!("#{tag}"), &["task-tag"]));
            }
            body.append(&tags);
        }
        if let Some(preview) = format::notes_preview(&t.notes) {
            let notes = text_label(&preview, &["task-notes"]);
            notes.set_ellipsize(gtk::pango::EllipsizeMode::End);
            body.append(&notes);
        }
        row.append(&body);

        let click = gtk::GestureClick::new();
        let ui = Rc::downgrade(self);
        click.connect_pressed(move |_, _, _, _| {
            if let Some(ui) = ui.upgrade() {
                ui.click_task(id);
            }
        });
        row.add_controller(click);
        row
    }

    /// Mark the selected row with `.selected-task`.
    pub fn apply_selection(&self) {
        let sel = self.state.borrow().task_sel;
        for (id, widget) in self.rows.borrow().iter() {
            set_class(widget, "selected-task", Some(*id) == sel);
        }
    }

    /// Scroll the selected row into view once layout has caught up.
    pub fn scroll_to_selected(self: &Rc<Self>) {
        let ui = self.clone();
        glib::idle_add_local_once(move || ui.scroll_to_selected_now());
    }

    fn scroll_to_selected_now(&self) {
        let (sel, first) = {
            let s = self.state.borrow();
            (s.task_sel, s.nav.first().copied())
        };
        let Some(sel) = sel else { return };
        let rows = self.rows.borrow();
        let Some((_, widget)) = rows.iter().find(|(id, _)| *id == sel) else {
            return;
        };
        let adj = self.w.task_scroll.vadjustment();
        if Some(sel) == first {
            adj.set_value(adj.lower());
            return;
        }
        let Some(p) = widget.compute_point(&self.w.task_box, &graphene::Point::new(0.0, 0.0))
        else {
            return;
        };
        let top = f64::from(p.y());
        let bottom = top + f64::from(widget.height());
        if top < adj.value() {
            adj.set_value(top);
        } else if bottom > adj.value() + adj.page_size() {
            adj.set_value(bottom - adj.page_size());
        }
    }
}
