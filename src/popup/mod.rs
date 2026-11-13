//! GTK4 + layer-shell popup (`APP_NAME popup`).
//!
//! Layout: `layout`/`format`/`form`/`undo` are GTK-free logic with unit tests;
//! `ui` holds the window and input handling, `render` builds the panes, `watch` reloads on DB changes.

mod form;
mod format;
pub mod hotzone;
mod layout;
mod render;
mod ui;
mod undo;
mod watch;

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;

use crate::app::APP_ID;
use crate::config::{Config, Theme};
use crate::paths;

/// Embedded default stylesheet; documents every CSS class the popup uses.
const DEFAULT_CSS: &str = include_str!("default.css");
/// Dark palette layered over `DEFAULT_CSS` for `Theme::Dark`.
const DARK_CSS: &str = include_str!("dark.css");

/// Run the popup, toggling an existing window unless `show` is requested.
pub fn run(show: bool) -> Result<()> {
    let cfg = Rc::new(Config::load()?);
    let app = gtk::Application::builder().application_id(APP_ID).build();
    let failure: Rc<RefCell<Option<anyhow::Error>>> = Rc::new(RefCell::new(None));

    let slot = failure.clone();
    let toggle_cfg = cfg.clone();
    app.connect_activate(move |app| {
        if let Err(e) = activate(app, &toggle_cfg, false) {
            *slot.borrow_mut() = Some(e);
            app.quit();
        }
    });

    let action = gio::SimpleAction::new("show", None);
    let slot = failure.clone();
    let weak_app = app.downgrade();
    action.connect_activate(move |_, _| {
        if let Some(app) = weak_app.upgrade() {
            if let Err(e) = activate(&app, &cfg, true) {
                *slot.borrow_mut() = Some(e);
                app.quit();
            }
        }
    });
    app.add_action(&action);
    app.register(None::<&gio::Cancellable>)?;
    if app.is_remote() {
        if show {
            app.activate_action("show", None);
        } else {
            app.activate();
        }
        if let Some(connection) = app.dbus_connection() {
            connection.flush_sync(None::<&gio::Cancellable>)?;
        }
        return Ok(());
    }

    // GTK must not parse our subcommand as its own options.
    let code = app.run_with_args::<&str>(&[]);
    if let Some(e) = failure.borrow_mut().take() {
        return Err(e);
    }
    if code != glib::ExitCode::SUCCESS {
        bail!("popup exited with status {}", code.get());
    }
    Ok(())
}

fn activate(app: &gtk::Application, cfg: &Config, show: bool) -> Result<()> {
    // Existing windows are only closed by toggle requests.
    if let Some(window) = app
        .active_window()
        .or_else(|| app.windows().into_iter().next())
    {
        if window.is_visible() && !show {
            window.close();
        } else if !window.is_visible() {
            window.present();
        }
        return Ok(());
    }

    load_css(cfg.popup.theme)?;
    let ui = ui::Ui::build(app, cfg)?;
    // Keep the watcher and UI state alive exactly as long as the window.
    let watcher = match paths::db_file().and_then(|p| watch::watch(&ui.window, p)) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("live reload disabled: {e:#}");
            None
        }
    };
    let keep = RefCell::new(Some((ui.clone(), watcher)));
    ui.window.connect_close_request(move |_| {
        keep.borrow_mut().take();
        glib::Propagation::Proceed
    });
    ui.window.present();
    Ok(())
}

/// Default stylesheet at application priority, the dark palette above it (`Theme::Dark`),
/// then the user's `style.css` on top. Also asks the GTK theme for the matching variant.
fn load_css(theme: Theme) -> Result<()> {
    let display = gdk::Display::default().context("no display available")?;

    if theme != Theme::System {
        gtk::Settings::for_display(&display)
            .set_gtk_application_prefer_dark_theme(theme == Theme::Dark);
    }

    let base = gtk::CssProvider::new();
    base.connect_parsing_error(|_, section, err| eprintln!("default stylesheet: {section}: {err}"));
    base.load_from_string(DEFAULT_CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &base,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    if theme == Theme::Dark {
        let dark = gtk::CssProvider::new();
        dark.connect_parsing_error(|_, section, err| {
            eprintln!("dark stylesheet: {section}: {err}")
        });
        dark.load_from_string(DARK_CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &dark,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
    }

    if let Ok(path) = paths::style_file() {
        if path.is_file() {
            let user = gtk::CssProvider::new();
            let shown = path.display().to_string();
            user.connect_parsing_error(move |_, section, err| {
                eprintln!("{shown}: {section}: {err}")
            });
            user.load_from_path(&path);
            gtk::style_context_add_provider_for_display(
                &display,
                &user,
                gtk::STYLE_PROVIDER_PRIORITY_USER,
            );
        }
    }
    Ok(())
}
