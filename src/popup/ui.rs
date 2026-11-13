//! The popup window: widgets, state, input handling, and user actions.
//! Rendering lives in `render.rs`; this file never builds list rows.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use super::format::{self, HINT};
use super::form::{self, FormInput};
use super::layout::{self, Step};
use super::undo::{Action, UndoStack};
use crate::app::APP_NAME;
use crate::capture;
use crate::config::Config;
use crate::db::{Db, TaskPatch};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    /// Quick-capture entry is open.
    Capture,
    /// Search entry is open (results are shown live).
    Search,
    /// Inline edit form for this task id.
    Edit(i64),
}

pub struct State {
    pub today: NaiveDate,
    /// First day of the month shown in the grid.
    pub month: NaiveDate,
    /// `None` = Today view.
    pub selected: Option<NaiveDate>,
    pub pane: Pane,
    pub mode: Mode,
    /// Task ids in keyboard order (only the rows currently visible).
    pub nav: Vec<i64>,
    pub task_sel: Option<i64>,
    pub inbox_open: bool,
    /// Active search text (also retained after Enter until Esc).
    pub search: Option<String>,
    pub undo: UndoStack,
    /// True while the status line shows a message instead of the key hint.
    pub status_dirty: bool,
}

pub struct DayCell {
    pub button: gtk::Button,
    pub number: gtk::Label,
    pub date: Cell<NaiveDate>,
}

pub struct EditForm {
    pub page: gtk::Box,
    pub title: gtk::Entry,
    pub date: gtk::Entry,
    pub time: gtk::Entry,
    pub priority: gtk::DropDown,
    pub tags: gtk::Entry,
    pub notes: gtk::TextView,
    pub error: gtk::Label,
    pub save: gtk::Button,
    pub cancel: gtk::Button,
}

pub struct Widgets {
    pub bar: gtk::Box,
    pub entry: gtk::Entry,
    pub chip: gtk::Label,
    pub left: gtk::Box,
    pub right: gtk::Box,
    pub prev: gtk::Button,
    pub next: gtk::Button,
    pub today_btn: gtk::Button,
    pub month_label: gtk::Label,
    pub cells: Vec<DayCell>,
    pub events_title: gtk::Label,
    pub events_box: gtk::Box,
    pub tasks_title: gtk::Label,
    pub stack: gtk::Stack,
    pub task_scroll: gtk::ScrolledWindow,
    pub task_box: gtk::Box,
    pub form: EditForm,
    pub status: gtk::Label,
}

pub struct Ui {
    pub window: gtk::ApplicationWindow,
    pub db: Db,
    pub state: RefCell<State>,
    pub w: Widgets,
    /// Task id → row widget, for the current render.
    pub rows: RefCell<Vec<(i64, gtk::Widget)>>,
}

pub fn set_class(w: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).css_classes(classes.to_vec()).build()
}

fn field(name: &str, w: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).build();
    row.append(&label(name, &["form-label"]));
    row.append(w);
    row
}

fn button(text: &str, classes: &[&str]) -> gtk::Button {
    gtk::Button::builder()
        .label(text)
        .focusable(false)
        .focus_on_click(false)
        .css_classes(classes.to_vec())
        .build()
}

impl Ui {
    pub fn build(app: &gtk::Application, cfg: &Config) -> Result<Rc<Ui>> {
        let db = Db::open_default()?;
        let today = Local::now().date_naive();

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .default_width(cfg.popup.width)
            .default_height(cfg.popup.height)
            .title(APP_NAME)
            .decorated(false)
            .css_classes(["popup-window"])
            .build();
        if gtk4_layer_shell::is_supported() {
            window.init_layer_shell();
            window.set_layer(Layer::Overlay);
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Right, true);
            window.set_margin(Edge::Top, cfg.popup.margin_top);
            window.set_margin(Edge::Right, cfg.popup.margin_right);
            window.set_keyboard_mode(KeyboardMode::OnDemand);
            window.set_namespace(Some(APP_NAME));
        } else {
            eprintln!("{APP_NAME}: compositor lacks wlr-layer-shell; showing a regular window");
        }

        let w = Self::build_widgets(&window);
        let ui = Rc::new(Ui {
            window,
            db,
            state: RefCell::new(State {
                today,
                month: layout::first_of_month(today),
                selected: None,
                pane: Pane::Right,
                mode: Mode::Normal,
                nav: Vec::new(),
                task_sel: None,
                inbox_open: true,
                search: None,
                undo: UndoStack::default(),
                status_dirty: false,
            }),
            w,
            rows: RefCell::new(Vec::new()),
        });
        ui.connect();
        ui.set_pane(Pane::Right);
        ui.refresh();
        Ok(ui)
    }

    fn build_widgets(window: &gtk::ApplicationWindow) -> Widgets {
        // ── input bar (capture / search) ──
        let entry = gtk::Entry::builder().hexpand(true).css_classes(["input-entry"]).build();
        let chip = label("", &["chip"]);
        chip.set_visible(false);
        let bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["input-bar"])
            .visible(false)
            .build();
        bar.append(&entry);
        bar.append(&chip);

        // ── left pane ──
        let prev = button("‹", &["flat", "month-nav"]);
        let next = button("›", &["flat", "month-nav"]);
        let today_btn = button("Today", &["flat", "today-button"]);
        let month_label = label("", &["month-label"]);
        month_label.set_hexpand(true);
        let header = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();
        header.append(&month_label);
        header.append(&today_btn);
        header.append(&prev);
        header.append(&next);

        let grid = gtk::Grid::builder().column_homogeneous(true).row_spacing(2).column_spacing(2).build();
        for (i, name) in layout::WEEKDAYS.iter().enumerate() {
            let l = gtk::Label::builder().label(*name).css_classes(["weekday"]).build();
            grid.attach(&l, i as i32, 0, 1, 1);
        }
        let cells: Vec<DayCell> = (0..layout::GRID_DAYS)
            .map(|i| {
                let number = gtk::Label::new(None);
                let dots = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).halign(gtk::Align::Center).spacing(2).build();
                dots.append(&gtk::Label::builder().label("•").css_classes(["dot", "dot-event"]).build());
                dots.append(&gtk::Label::builder().label("•").css_classes(["dot", "dot-task"]).build());
                let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).halign(gtk::Align::Center).build();
                content.append(&number);
                content.append(&dots);
                let button = gtk::Button::builder()
                    .child(&content)
                    .focusable(false)
                    .focus_on_click(false)
                    .css_classes(["flat", "day"])
                    .build();
                grid.attach(&button, (i % 7) as i32, (i / 7) as i32 + 1, 1, 1);
                DayCell { button, number, date: Cell::new(NaiveDate::MIN) }
            })
            .collect();

        let events_title = label("", &["pane-title"]);
        let events_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        let events_scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&events_box)
            .build();

        let left = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .width_request(290)
            .css_classes(["pane", "left-pane"])
            .build();
        left.append(&header);
        left.append(&grid);
        left.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        left.append(&events_title);
        left.append(&events_scroll);

        // ── right pane ──
        let tasks_title = label("", &["pane-title"]);
        let task_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        let task_scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&task_box)
            .build();
        let form = Self::build_form();
        let stack = gtk::Stack::new();
        stack.add_named(&task_scroll, Some("list"));
        stack.add_named(&form.page, Some("edit"));
        stack.set_visible_child_name("list");
        let right = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .hexpand(true)
            .css_classes(["pane", "right-pane"])
            .build();
        right.append(&tasks_title);
        right.append(&stack);

        // ── assembly ──
        let panes = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).vexpand(true).build();
        panes.append(&left);
        panes.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        panes.append(&right);
        let status = label(HINT, &["status"]);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["root"]).build();
        root.append(&bar);
        root.append(&panes);
        root.append(&status);
        window.set_child(Some(&root));

        Widgets {
            bar,
            entry,
            chip,
            left,
            right,
            prev,
            next,
            today_btn,
            month_label,
            cells,
            events_title,
            events_box,
            tasks_title,
            stack,
            task_scroll,
            task_box,
            form,
            status,
        }
    }

    fn build_form() -> EditForm {
        let title = gtk::Entry::builder().placeholder_text("Title").build();
        let date = gtk::Entry::builder().placeholder_text("YYYY-MM-DD or “tomorrow”").build();
        let time = gtk::Entry::builder().placeholder_text("HH:MM").build();
        let priority = gtk::DropDown::from_strings(&form::PRIORITY_NAMES);
        let tags = gtk::Entry::builder().placeholder_text("space-separated").build();
        let notes = gtk::TextView::builder().wrap_mode(gtk::WrapMode::WordChar).css_classes(["notes-view"]).build();
        let notes_scroll = gtk::ScrolledWindow::builder().vexpand(true).min_content_height(80).child(&notes).build();
        let error = label("", &["form-error"]);
        error.set_visible(false);
        let save = button("Save", &["suggested-action"]);
        let cancel = button("Cancel", &[]);

        let when = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        when.append(&field("Due date", &date));
        when.append(&field("Time", &time));
        when.append(&field("Priority", &priority));
        let buttons = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).halign(gtk::Align::End).build();
        buttons.append(&cancel);
        buttons.append(&save);

        let page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).css_classes(["edit-form"]).build();
        page.append(&field("Title", &title));
        page.append(&when);
        page.append(&field("Tags", &tags));
        page.append(&label("Notes", &["form-label"]));
        page.append(&notes_scroll);
        page.append(&error);
        page.append(&buttons);
        page.append(&label("Enter saves · Ctrl+Enter saves from notes · Esc cancels", &["form-hint"]));
        EditForm { page, title, date, time, priority, tags, notes, error, save, cancel }
    }

    // ───────────────────────────── wiring ─────────────────────────────

    fn connect(self: &Rc<Self>) {
        // Keys: capture phase so list/button widgets never swallow navigation keys.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let ui = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| match ui.upgrade() {
            Some(ui) => ui.on_key(key, mods),
            None => glib::Propagation::Proceed,
        });
        self.window.add_controller(keys);

        // Close on focus loss, but only once the window has actually been focused.
        let was_active = Cell::new(false);
        self.window.connect_is_active_notify(move |w| {
            if w.is_active() {
                was_active.set(true);
            } else if was_active.get() {
                w.close();
            }
        });

        // Live reload entry point for the DB watcher.
        let reload = gio::SimpleAction::new("reload", None);
        let ui = Rc::downgrade(self);
        reload.connect_activate(move |_, _| {
            if let Some(ui) = ui.upgrade() {
                ui.refresh();
            }
        });
        self.window.add_action(&reload);

        for (i, cell) in self.w.cells.iter().enumerate() {
            let ui = Rc::downgrade(self);
            cell.button.connect_clicked(move |_| {
                if let Some(ui) = ui.upgrade() {
                    ui.click_day(i);
                }
            });
        }
        let ui = Rc::downgrade(self);
        self.w.prev.connect_clicked(move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.shift_view_month(-1);
            }
        });
        let ui = Rc::downgrade(self);
        self.w.next.connect_clicked(move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.shift_view_month(1);
            }
        });
        let ui = Rc::downgrade(self);
        self.w.today_btn.connect_clicked(move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.goto_today();
            }
        });

        let ui = Rc::downgrade(self);
        self.w.entry.connect_changed(move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.on_entry_changed();
            }
        });
        let ui = Rc::downgrade(self);
        self.w.entry.connect_activate(move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.on_entry_activate();
            }
        });

        let f = &self.w.form;
        for e in [&f.title, &f.date, &f.time, &f.tags] {
            let ui = Rc::downgrade(self);
            e.connect_activate(move |_| {
                if let Some(ui) = ui.upgrade() {
                    ui.save_edit();
                }
            });
        }
        let ui = Rc::downgrade(self);
        f.save.connect_clicked(move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.save_edit();
            }
        });
        let ui = Rc::downgrade(self);
        f.cancel.connect_clicked(move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.cancel_edit();
            }
        });
    }

    // ───────────────────────────── status / panes ─────────────────────────────

    pub fn set_status(&self, msg: &str, error: bool) {
        self.w.status.set_text(msg);
        set_class(&self.w.status, "error", error);
        self.state.borrow_mut().status_dirty = true;
    }

    fn reset_status(&self) {
        if std::mem::take(&mut self.state.borrow_mut().status_dirty) {
            self.w.status.set_text(HINT);
            self.w.status.remove_css_class("error");
        }
    }

    pub fn set_pane(&self, pane: Pane) {
        self.state.borrow_mut().pane = pane;
        set_class(&self.w.left, "focused", pane == Pane::Left);
        set_class(&self.w.right, "focused", pane == Pane::Right);
    }

    fn toggle_pane(&self) {
        let next = match self.state.borrow().pane {
            Pane::Left => Pane::Right,
            Pane::Right => Pane::Left,
        };
        self.set_pane(next);
    }

    // ───────────────────────────── keyboard ─────────────────────────────

    fn on_key(self: &Rc<Self>, key: gdk::Key, mods: gdk::ModifierType) -> glib::Propagation {
        let mode = self.state.borrow().mode;
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
        match mode {
            Mode::Normal => self.on_key_normal(key, mods),
            Mode::Capture | Mode::Search => match key {
                gdk::Key::Escape => {
                    self.cancel_input();
                    glib::Propagation::Stop
                }
                gdk::Key::Tab | gdk::Key::ISO_Left_Tab => glib::Propagation::Stop,
                _ => glib::Propagation::Proceed,
            },
            Mode::Edit(_) => match key {
                gdk::Key::Escape => {
                    self.cancel_edit();
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter if ctrl => {
                    self.save_edit();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            },
        }
    }

    fn on_key_normal(self: &Rc<Self>, key: gdk::Key, mods: gdk::ModifierType) -> glib::Propagation {
        if mods.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK) {
            return glib::Propagation::Proceed;
        }
        self.reset_status();
        let handled = match key {
            gdk::Key::Escape => {
                self.escape();
                true
            }
            gdk::Key::Tab | gdk::Key::ISO_Left_Tab => {
                self.toggle_pane();
                true
            }
            gdk::Key::t => {
                self.goto_today();
                true
            }
            gdk::Key::u => {
                self.undo();
                true
            }
            gdk::Key::N => {
                self.enter_capture();
                true
            }
            gdk::Key::slash => {
                self.enter_search();
                true
            }
            gdk::Key::Page_Up => {
                self.page_month(-1);
                true
            }
            gdk::Key::Page_Down => {
                self.page_month(1);
                true
            }
            _ => {
                let pane = self.state.borrow().pane;
                match pane {
                    Pane::Left => self.on_key_left(key),
                    Pane::Right => self.on_key_right(key),
                }
            }
        };
        if handled {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    }

    fn on_key_left(self: &Rc<Self>, key: gdk::Key) -> bool {
        let step = match key {
            gdk::Key::h | gdk::Key::Left => Step::Left,
            gdk::Key::l | gdk::Key::Right => Step::Right,
            gdk::Key::k | gdk::Key::Up => Step::Up,
            gdk::Key::j | gdk::Key::Down => Step::Down,
            _ => return false,
        };
        self.move_selected_day(step);
        true
    }

    fn on_key_right(self: &Rc<Self>, key: gdk::Key) -> bool {
        match key {
            gdk::Key::j | gdk::Key::Down => self.step_task(1),
            gdk::Key::k | gdk::Key::Up => self.step_task(-1),
            gdk::Key::x => self.toggle_selected_complete(),
            gdk::Key::e => self.enter_edit(),
            gdk::Key::d => self.trash_selected(),
            gdk::Key::i => {
                {
                    let mut s = self.state.borrow_mut();
                    s.inbox_open = !s.inbox_open;
                }
                self.rerender_tasks();
            }
            _ => return false,
        }
        true
    }

    fn escape(self: &Rc<Self>) {
        let searching = self.state.borrow().search.is_some();
        if searching {
            self.state.borrow_mut().search = None;
            self.refresh();
        } else {
            self.window.close();
        }
    }

    // ───────────────────────────── calendar navigation ─────────────────────────────

    fn move_selected_day(self: &Rc<Self>, step: Step) {
        let (base, month) = {
            let s = self.state.borrow();
            (s.selected.unwrap_or(s.today), s.month)
        };
        let next = layout::move_day(base, step);
        {
            let mut s = self.state.borrow_mut();
            s.selected = Some(next);
            s.search = None;
            if layout::first_of_month(next) != month {
                s.month = layout::first_of_month(next);
            }
        }
        self.refresh();
    }

    /// PageUp/PageDown: shift the selected day (or just the view, with no selection) by a month.
    fn page_month(self: &Rc<Self>, n: i32) {
        {
            let mut s = self.state.borrow_mut();
            match s.selected {
                Some(d) => {
                    let moved = layout::shift_months(d, n);
                    s.selected = Some(moved);
                    s.month = layout::first_of_month(moved);
                    s.search = None;
                }
                None => s.month = layout::shift_months(s.month, n),
            }
        }
        self.refresh();
    }

    fn shift_view_month(self: &Rc<Self>, n: i32) {
        {
            let mut s = self.state.borrow_mut();
            s.month = layout::shift_months(s.month, n);
        }
        if let Err(e) = self.render_calendar() {
            self.set_status(&format!("Database error: {e:#}"), true);
        }
    }

    fn click_day(self: &Rc<Self>, index: usize) {
        let date = self.w.cells[index].date.get();
        {
            let mut s = self.state.borrow_mut();
            // Clicking the selected day again returns to the Today view.
            s.selected = if s.selected == Some(date) { None } else { Some(date) };
            s.month = layout::first_of_month(date);
            s.search = None;
        }
        self.set_pane(Pane::Left);
        self.refresh();
    }

    fn goto_today(self: &Rc<Self>) {
        {
            let mut s = self.state.borrow_mut();
            s.today = Local::now().date_naive();
            s.selected = None;
            s.month = layout::first_of_month(s.today);
            s.search = None;
        }
        self.refresh();
    }

    // ───────────────────────────── task selection & actions ─────────────────────────────

    fn step_task(self: &Rc<Self>, delta: i32) {
        {
            let mut s = self.state.borrow_mut();
            s.task_sel = layout::step_selection(&s.nav, s.task_sel, delta);
        }
        self.apply_selection();
        self.scroll_to_selected();
    }

    pub fn click_task(self: &Rc<Self>, id: i64) {
        self.state.borrow_mut().task_sel = Some(id);
        self.set_pane(Pane::Right);
        self.apply_selection();
    }

    pub fn selected_task_id(&self) -> Option<i64> {
        self.state.borrow().task_sel
    }

    pub fn toggle_selected_complete(self: &Rc<Self>) {
        if let Some(id) = self.selected_task_id() {
            self.toggle_complete(id);
        }
    }

    pub fn toggle_complete(self: &Rc<Self>, id: i64) {
        match self.db.get_task(id) {
            Ok(Some(t)) => {
                let (action, verb) = if t.completed_at.is_some() {
                    (Action::Reopen(id), "Reopened")
                } else {
                    (Action::Complete(id), "Completed")
                };
                self.perform(action, format!("{verb} {}", format::quoted(&t.title)));
            }
            Ok(None) => self.refresh(),
            Err(e) => self.set_status(&format!("Database error: {e:#}"), true),
        }
    }

    fn trash_selected(self: &Rc<Self>) {
        let Some(id) = self.selected_task_id() else { return };
        match self.db.get_task(id) {
            Ok(Some(t)) => self.perform(Action::Delete(id), format!("Trashed {}", format::quoted(&t.title))),
            Ok(None) => self.refresh(),
            Err(e) => self.set_status(&format!("Database error: {e:#}"), true),
        }
    }

    /// Apply `action`, remember its inverse for undo, refresh.
    fn perform(self: &Rc<Self>, action: Action, label: String) {
        let result = (|| -> Result<()> {
            let before = self.db.get_task(action.task_id())?.context("task no longer exists")?;
            let inverse = action.inverse(&before);
            action.apply(&self.db)?;
            self.state.borrow_mut().undo.push(label.clone(), inverse);
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.refresh();
                self.set_status(&format!("{label} — u to undo"), false);
            }
            Err(e) => self.set_status(&format!("{e:#}"), true),
        }
    }

    fn undo(self: &Rc<Self>) {
        let popped = self.state.borrow_mut().undo.pop();
        let Some(entry) = popped else {
            self.set_status("Nothing to undo", false);
            return;
        };
        match entry.action.apply(&self.db) {
            Ok(task) => {
                self.state.borrow_mut().task_sel = Some(task.id);
                self.refresh();
                self.set_status(&format!("Undid: {}", entry.label), false);
            }
            Err(e) => self.set_status(&format!("Undo failed: {e:#}"), true),
        }
    }

    // ───────────────────────────── input bar: capture / search ─────────────────────────────

    fn show_bar(&self, placeholder: &str) {
        self.w.entry.set_placeholder_text(Some(placeholder));
        self.w.entry.set_text("");
        self.w.chip.set_visible(false);
        self.w.bar.set_visible(true);
        self.w.entry.grab_focus();
    }

    fn hide_bar(&self) {
        self.w.bar.set_visible(false);
        self.w.entry.set_text("");
        GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
    }

    fn enter_capture(self: &Rc<Self>) {
        self.state.borrow_mut().mode = Mode::Capture;
        self.show_bar("New task — e.g. call mom fri 5pm #family !high");
    }

    fn enter_search(self: &Rc<Self>) {
        {
            let mut s = self.state.borrow_mut();
            s.mode = Mode::Search;
            s.search = Some(String::new());
        }
        self.show_bar("Search tasks and events…");
        self.set_pane(Pane::Right);
        self.refresh();
    }

    /// Esc inside the bar: drop the input (and, for search, the filter).
    fn cancel_input(self: &Rc<Self>) {
        {
            let mut s = self.state.borrow_mut();
            if s.mode == Mode::Search {
                s.search = None;
            }
            s.mode = Mode::Normal;
        }
        self.hide_bar();
        self.refresh();
    }

    fn on_entry_changed(self: &Rc<Self>) {
        let text = self.w.entry.text().to_string();
        let mode = self.state.borrow().mode;
        match mode {
            Mode::Capture => {
                let cap = capture::parse(&text, Local::now().naive_local());
                let preview = cap.preview();
                self.w.chip.set_text(&preview);
                self.w.chip.set_visible(!preview.is_empty());
                set_class(&self.w.chip, "invalid", cap.title.trim().is_empty());
            }
            Mode::Search => {
                self.state.borrow_mut().search = Some(text);
                self.refresh_search_views();
            }
            Mode::Normal | Mode::Edit(_) => {}
        }
    }

    fn on_entry_activate(self: &Rc<Self>) {
        let mode = self.state.borrow().mode;
        match mode {
            Mode::Capture => self.submit_capture(),
            Mode::Search => self.finish_search(),
            Mode::Normal | Mode::Edit(_) => {}
        }
    }

    fn refresh_search_views(self: &Rc<Self>) {
        let result = self.render_events().and_then(|()| self.render_tasks());
        if let Err(e) = result {
            self.set_status(&format!("Database error: {e:#}"), true);
        }
    }

    /// Enter in the search bar: keep the results, hand the keyboard back to the list.
    fn finish_search(self: &Rc<Self>) {
        {
            let mut s = self.state.borrow_mut();
            s.mode = Mode::Normal;
            if s.search.as_deref().is_some_and(|q| q.trim().is_empty()) {
                s.search = None;
            }
        }
        self.hide_bar();
        self.set_pane(Pane::Right);
        self.refresh();
    }

    fn submit_capture(self: &Rc<Self>) {
        let text = self.w.entry.text().to_string();
        let cap = capture::parse(&text, Local::now().naive_local());
        if cap.title.trim().is_empty() {
            self.set_status("Nothing to add — type a title", true);
            return;
        }
        let new = cap.into_new_task();
        let title = new.title.clone();
        match self.db.create_task(new) {
            Ok(task) => {
                {
                    let mut s = self.state.borrow_mut();
                    s.mode = Mode::Normal;
                    s.task_sel = Some(task.id);
                    s.undo.push(format!("Added {}", format::quoted(&title)), Action::Delete(task.id));
                }
                self.hide_bar();
                self.set_pane(Pane::Right);
                self.refresh();
                let when = task.due_date.map(|d| format!(" (due {})", d.format("%a %-d %b"))).unwrap_or_default();
                self.set_status(&format!("Added {}{when} — u to undo", format::quoted(&title)), false);
            }
            Err(e) => self.set_status(&format!("{e:#}"), true),
        }
    }

    // ───────────────────────────── edit form ─────────────────────────────

    fn enter_edit(self: &Rc<Self>) {
        let Some(id) = self.selected_task_id() else { return };
        let task = match self.db.get_task(id) {
            Ok(Some(t)) => t,
            Ok(None) => return self.refresh(),
            Err(e) => return self.set_status(&format!("Database error: {e:#}"), true),
        };
        let input = FormInput::from_task(&task);
        let f = &self.w.form;
        f.title.set_text(&input.title);
        f.date.set_text(&input.date);
        f.time.set_text(&input.time);
        f.tags.set_text(&input.tags);
        f.priority.set_selected(form::priority_index(input.priority));
        f.notes.buffer().set_text(&input.notes);
        f.error.set_visible(false);
        self.state.borrow_mut().mode = Mode::Edit(id);
        self.w.tasks_title.set_text("Edit task");
        self.w.stack.set_visible_child_name("edit");
        f.title.grab_focus();
        f.title.set_position(-1);
    }

    fn leave_edit(self: &Rc<Self>) {
        self.state.borrow_mut().mode = Mode::Normal;
        self.w.stack.set_visible_child_name("list");
        GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        self.refresh();
    }

    fn cancel_edit(self: &Rc<Self>) {
        self.leave_edit();
    }

    fn read_form(&self) -> FormInput {
        let f = &self.w.form;
        let buf = f.notes.buffer();
        let (a, b) = buf.bounds();
        FormInput {
            title: f.title.text().to_string(),
            date: f.date.text().to_string(),
            time: f.time.text().to_string(),
            priority: form::priority_at(f.priority.selected()),
            tags: f.tags.text().to_string(),
            notes: buf.text(&a, &b, false).to_string(),
        }
    }

    fn save_edit(self: &Rc<Self>) {
        let mode = self.state.borrow().mode;
        let Mode::Edit(id) = mode else { return };
        let fail = |msg: &str| {
            self.w.form.error.set_text(msg);
            self.w.form.error.set_visible(true);
        };
        let task = match self.db.get_task(id) {
            Ok(Some(t)) => t,
            Ok(None) => return fail("This task no longer exists"),
            Err(e) => return fail(&format!("{e:#}")),
        };
        let patch: Option<TaskPatch> = match form::build_patch(&task, &self.read_form(), Local::now().naive_local()) {
            Ok(p) => p,
            Err(msg) => return fail(&msg),
        };
        let Some(patch) = patch else {
            return self.leave_edit();
        };
        let action = Action::Patch(id, patch);
        let inverse = action.inverse(&task);
        match action.apply(&self.db) {
            Ok(updated) => {
                self.state.borrow_mut().undo.push(format!("Edited {}", format::quoted(&task.title)), inverse);
                self.leave_edit();
                self.set_status(&format!("Edited {} — u to undo", format::quoted(&updated.title)), false);
            }
            Err(e) => fail(&format!("{e:#}")),
        }
    }
}
