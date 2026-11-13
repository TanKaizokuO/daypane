//! Transparent layer-shell hover target for a centered clock.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::app::{APP_ID, APP_NAME};
use crate::config::Config;

pub fn run() -> Result<()> {
    let cfg = Config::load()?.hover;
    if !cfg.enabled {
        println!("hover zone disabled in configuration");
        return Ok(());
    }

    let app = gtk::Application::builder()
        .application_id(format!("{APP_ID}.Hotzone"))
        .build();
    app.register(None::<&gio::Cancellable>)?;
    if app.is_remote() {
        return Ok(());
    }

    let failure = Rc::new(RefCell::new(None));
    let slot = failure.clone();
    app.connect_activate(move |app| {
        if !gtk4_layer_shell::is_supported() {
            *slot.borrow_mut() = Some(anyhow::anyhow!("compositor lacks wlr-layer-shell"));
            app.quit();
            return;
        }
        let Some(display) = gdk::Display::default() else {
            *slot.borrow_mut() = Some(anyhow::anyhow!("no display available"));
            app.quit();
            return;
        };
        let css = gtk::CssProvider::new();
        css.load_from_string("window.hover-zone { background: transparent; box-shadow: none; }");
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // Layer surfaces size to their content, so the hit area is a fixed-size child.
        let area = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        area.set_size_request(cfg.width, cfg.height);
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .decorated(false)
            .css_classes(["hover-zone"])
            .child(&area)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Top, true);
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_namespace(Some(&format!("{APP_NAME}-hotzone")));

        let motion = gtk::EventControllerMotion::new();
        let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let enter_timer = timer.clone();
        motion.connect_enter(move |_, _, _| {
            if let Some(id) = enter_timer.borrow_mut().take() {
                id.remove();
            }
            let fired_timer = enter_timer.clone();
            let id = glib::timeout_add_local_once(Duration::from_millis(cfg.delay_ms), move || {
                fired_timer.borrow_mut().take();
                match std::env::current_exe()
                    .context("finding executable")
                    .and_then(|exe| {
                        std::process::Command::new(exe)
                            .args(["popup", "--show"])
                            .spawn()
                            .context("starting popup")
                    }) {
                    Ok(mut child) => {
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                    }
                    Err(e) => eprintln!("{e:#}"),
                }
            });
            *enter_timer.borrow_mut() = Some(id);
        });
        motion.connect_leave(move |_| {
            if let Some(id) = timer.borrow_mut().take() {
                id.remove();
            }
        });
        window.add_controller(motion);
        window.present();
    });
    let code = app.run_with_args::<&str>(&[]);
    if let Some(e) = failure.borrow_mut().take() {
        return Err(e);
    }
    if code != glib::ExitCode::SUCCESS {
        bail!("hotzone exited with status {}", code.get());
    }
    Ok(())
}
