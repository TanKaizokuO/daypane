//! `install`: render the systemd user unit, `.desktop` file, default config, and Waybar snippets.
//!
//! All files come from the templates in `assets/` (embedded with `include_str!`); the app name
//! never appears literally in them, only as `@PLACEHOLDER@`s filled from `crate::app`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::app::{APP_ID, APP_NAME, DAEMON_UNIT, WAYBAR_MODULE};
use crate::config::Config;
use crate::paths;

const UNIT_TEMPLATE: &str = include_str!("../assets/daemon.service.in");
const DESKTOP_TEMPLATE: &str = include_str!("../assets/app.desktop.in");
const WAYBAR_MODULE_TEMPLATE: &str = include_str!("../assets/waybar-module.json.in");
const WAYBAR_CSS_TEMPLATE: &str = include_str!("../assets/waybar-style.css.in");
const CONFIG_TEMPLATE: &str = include_str!("../assets/config.toml.in");

#[derive(clap::Args)]
pub struct InstallArgs {
    /// Print the rendered files and instructions; write nothing and run nothing.
    #[arg(long)]
    print: bool,
    /// Skip the systemd user unit; print an `exec` line for your compositor instead.
    #[arg(long)]
    no_systemd: bool,
}

/// Placeholder values for template rendering.
#[derive(Debug, Default, Clone)]
struct Vars(Vec<(&'static str, String)>);

impl Vars {
    fn with(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.0.push((key, value.into()));
        self
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The same variables, escaped for embedding inside a JSON string literal.
    fn json_escaped(&self) -> Self {
        Self(
            self.0
                .iter()
                .map(|(k, v)| {
                    let quoted = serde_json::to_string(v).expect("string serializes");
                    (*k, quoted[1..quoted.len() - 1].to_owned())
                })
                .collect(),
        )
    }

    /// Variables shared by the unit, desktop file, and Waybar snippets.
    fn for_app(exec: &Path, waybar_file: &Path, signal: u8) -> Self {
        Self::default()
            .with("APP_NAME", APP_NAME)
            .with("APP_TITLE", capitalize(APP_NAME))
            .with("APP_ID", APP_ID)
            .with("EXEC", shell_quote(&exec.to_string_lossy()))
            .with("WAYBAR_FILE", shell_quote(&waybar_file.to_string_lossy()))
            .with("SIGNAL", signal.to_string())
    }

    /// Variables for the commented default `config.toml`, taken from `Config::default()`.
    fn for_config(c: &Config) -> Self {
        Self::default()
            .with("APP_NAME", APP_NAME)
            .with("POPUP_POSITION", c.popup.position.as_str())
            .with("POPUP_MARGIN_TOP", c.popup.margin_top.to_string())
            .with("POPUP_MARGIN_RIGHT", c.popup.margin_right.to_string())
            .with("POPUP_WIDTH", c.popup.width.to_string())
            .with("POPUP_HEIGHT", c.popup.height.to_string())
            .with("POPUP_THEME", c.popup.theme.as_str())
            .with("HOVER_ENABLED", c.hover.enabled.to_string())
            .with("HOVER_WIDTH", c.hover.width.to_string())
            .with("HOVER_HEIGHT", c.hover.height.to_string())
            .with("HOVER_DELAY_MS", c.hover.delay_ms.to_string())
            .with(
                "REMINDERS_TASK_OFFSET_MINUTES",
                c.reminders.task_offset_minutes.to_string(),
            )
            .with(
                "REMINDERS_SNOOZE_MINUTES",
                c.reminders.snooze_minutes.to_string(),
            )
            .with(
                "GOOGLE_SYNC_INTERVAL_SECONDS",
                c.google.sync_interval_seconds.to_string(),
            )
            .with("WAYBAR_SIGNAL", c.waybar.signal.to_string())
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Quote for a shell / systemd / .desktop `Exec` word only when needed.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '$' | '`'));
    if plain {
        return s.to_owned();
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Replace every `@NAME@` (`NAME` = `[A-Z0-9_]+`) with its value. An unknown placeholder is an
/// error; an `@` that does not start a placeholder is kept literally.
fn render(template: &str, vars: &Vars) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let name = after.find('@').map(|end| &after[..end]).filter(|n| {
            !n.is_empty()
                && n.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        });
        match name {
            Some(name) => {
                let value = vars
                    .get(name)
                    .with_context(|| format!("unknown template placeholder @{name}@"))?;
                out.push_str(value);
                rest = &after[name.len() + 1..];
            }
            None => {
                out.push('@');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Everything `install` produces, rendered.
struct Rendered {
    unit: String,
    desktop: String,
    waybar_module: String,
    waybar_css: String,
    config: String,
}

fn render_all(exec: &Path, waybar_file: &Path, signal: u8) -> Result<Rendered> {
    let vars = Vars::for_app(exec, waybar_file, signal);
    Ok(Rendered {
        unit: render(UNIT_TEMPLATE, &vars)?,
        desktop: render(DESKTOP_TEMPLATE, &vars)?,
        waybar_module: render(WAYBAR_MODULE_TEMPLATE, &vars.json_escaped())?,
        waybar_css: render(WAYBAR_CSS_TEMPLATE, &vars)?,
        config: render(CONFIG_TEMPLATE, &Vars::for_config(&Config::default()))?,
    })
}

/// Where the installed files go.
struct Targets {
    unit: PathBuf,
    desktop: PathBuf,
    config_dir: PathBuf,
    config_file: PathBuf,
}

impl Targets {
    fn from_xdg() -> Result<Self> {
        let config_home = dirs::config_dir().context("cannot determine XDG config directory")?;
        let data_home = dirs::data_dir().context("cannot determine XDG data directory")?;
        Ok(Self {
            unit: config_home.join("systemd/user").join(DAEMON_UNIT),
            desktop: data_home
                .join("applications")
                .join(format!("{APP_ID}.desktop")),
            config_dir: paths::config_dir()?,
            config_file: paths::config_file()?,
        })
    }
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

/// Write the files. The unit is skipped when `with_unit` is false; the config only when absent.
/// Returns the paths written, and whether a config was created.
fn write_files(t: &Targets, r: &Rendered, with_unit: bool) -> Result<(Vec<PathBuf>, bool)> {
    let mut written = Vec::new();
    if with_unit {
        write_file(&t.unit, &r.unit)?;
        written.push(t.unit.clone());
    }
    write_file(&t.desktop, &r.desktop)?;
    written.push(t.desktop.clone());

    std::fs::create_dir_all(&t.config_dir)
        .with_context(|| format!("creating {}", t.config_dir.display()))?;
    let config_created = !t.config_file.exists();
    if config_created {
        write_file(&t.config_file, &r.config)?;
        written.push(t.config_file.clone());
    }
    Ok((written, config_created))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("failed to run `systemctl` (is systemd available?)")?;
    if !status.success() {
        bail!("`systemctl --user {}` exited with {status}", args.join(" "));
    }
    Ok(())
}

fn print_section(title: &str, body: &str) {
    println!("# ── {title} ──");
    println!("{}", body.trim_end());
    println!();
}

fn print_waybar_help(r: &Rendered) {
    print_section(
        "Waybar module (add to your Waybar config)",
        &r.waybar_module,
    );
    println!(
        "Then list \"{WAYBAR_MODULE}\" in \"modules-right\" (or any modules-* array) of the same config,\n\
         e.g.  \"modules-right\": [\"{WAYBAR_MODULE}\", \"clock\"],\n"
    );
    print_section(
        "Waybar style.css (add to ~/.config/waybar/style.css)",
        &r.waybar_css,
    );
}

fn print_exec_fallback(exec: &Path) {
    let exec = shell_quote(&exec.to_string_lossy());
    println!("# ── Starting the daemon without systemd ──");
    println!("Run the daemon from your compositor / session startup instead, e.g.:");
    println!("  sway / i3:   exec {exec} daemon");
    println!("  Hyprland:    exec-once = {exec} daemon");
    println!("  any shell:   {exec} daemon &");
    println!();
}

fn print_hotzone_help(exec: &Path) {
    let exec = shell_quote(&exec.to_string_lossy());
    println!("# ── Hover zone (add to your sway config) ──");
    println!("  exec {exec} hotzone\n");
}

pub fn run(args: InstallArgs) -> Result<()> {
    let exec = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("cannot determine the path of the running executable")?;
    let config = Config::load()?;
    let rendered = render_all(&exec, &paths::waybar_file()?, config.waybar.signal)?;
    let targets = Targets::from_xdg()?;
    let with_unit = !args.no_systemd;

    if args.print {
        if with_unit {
            print_section(
                &format!("systemd user unit → {}", targets.unit.display()),
                &rendered.unit,
            );
        }
        print_section(
            &format!("desktop entry → {}", targets.desktop.display()),
            &rendered.desktop,
        );
        print_section(
            &format!(
                "default config (written only if absent) → {}",
                targets.config_file.display()
            ),
            &rendered.config,
        );
        if !with_unit {
            print_exec_fallback(&exec);
        }
        print_waybar_help(&rendered);
        print_hotzone_help(&exec);
        return Ok(());
    }

    let (written, config_created) = write_files(&targets, &rendered, with_unit)?;
    for p in &written {
        println!("wrote {}", p.display());
    }
    if !config_created {
        println!("kept existing {}", targets.config_file.display());
    }

    let mut systemd_error = None;
    if with_unit {
        let result = systemctl(&["daemon-reload"])
            .and_then(|()| systemctl(&["enable", "--now", DAEMON_UNIT]));
        match result {
            Ok(()) => println!("enabled and started {DAEMON_UNIT}"),
            Err(e) => {
                eprintln!("warning: could not enable {DAEMON_UNIT}: {e:#}");
                eprintln!(
                    "         the unit file is installed; run `systemctl --user daemon-reload && \
                     systemctl --user enable --now {DAEMON_UNIT}` yourself, or re-run with --no-systemd."
                );
                systemd_error = Some(e);
            }
        }
    } else {
        println!();
        print_exec_fallback(&exec);
    }
    println!();

    if config_created {
        println!(
            "Next: set your Google OAuth client in {} ([google] client_id / client_secret), \
             then run `{APP_NAME} login`.\n",
            targets.config_file.display()
        );
    }
    print_waybar_help(&rendered);
    print_hotzone_help(&exec);

    match systemd_error {
        Some(e) => Err(e.context(format!(
            "files were installed, but {DAEMON_UNIT} was not enabled"
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_vars() -> Vars {
        Vars::for_app(
            Path::new("/opt/bin/demo"),
            Path::new("/run/user/1000/demo/waybar.json"),
            9,
        )
    }

    #[test]
    fn render_replaces_all_placeholders() {
        let v = Vars::default().with("A", "1").with("B_2", "two");
        assert_eq!(render("x @A@ y @B_2@ @A@", &v).unwrap(), "x 1 y two 1");
    }

    #[test]
    fn render_unknown_placeholder_is_an_error() {
        let err = render("hello @NOPE@", &Vars::default()).unwrap_err();
        assert!(err.to_string().contains("@NOPE@"), "{err}");
    }

    #[test]
    fn render_keeps_stray_at_signs() {
        let v = Vars::default().with("A", "1");
        assert_eq!(
            render("me@example.com @A@ @", &v).unwrap(),
            "me@example.com 1 @"
        );
        assert_eq!(render("@@A@", &v).unwrap(), "@1");
    }

    #[test]
    fn every_template_renders_without_leftovers() {
        let r = render_all(
            Path::new("/opt/bin/demo"),
            Path::new("/run/user/1000/demo/waybar.json"),
            9,
        )
        .unwrap();
        for (name, text) in [
            ("unit", &r.unit),
            ("desktop", &r.desktop),
            ("module", &r.waybar_module),
            ("css", &r.waybar_css),
            ("config", &r.config),
        ] {
            assert!(!text.contains('@'), "{name} still has an '@': {text}");
        }
    }

    #[test]
    fn unit_has_required_directives() {
        let u = render(UNIT_TEMPLATE, &demo_vars()).unwrap();
        for line in [
            "Type=simple",
            "ExecStart=/opt/bin/demo daemon",
            "Restart=on-failure",
            "After=graphical-session.target",
            "PartOf=graphical-session.target",
            "WantedBy=graphical-session.target",
        ] {
            assert!(u.lines().any(|l| l == line), "missing `{line}` in:\n{u}");
        }
    }

    #[test]
    fn desktop_entry_is_hidden_and_launches_popup() {
        let d = render(DESKTOP_TEMPLATE, &demo_vars()).unwrap();
        assert!(d.lines().any(|l| l == "Exec=/opt/bin/demo popup"));
        assert!(d.lines().any(|l| l == "NoDisplay=true"));
        assert!(d
            .lines()
            .any(|l| l == format!("Name={}", capitalize(APP_NAME))));
        assert!(d.contains(&format!("StartupWMClass={APP_ID}")));
    }

    #[test]
    fn waybar_module_is_valid_json_even_with_awkward_paths() {
        let vars = Vars::for_app(
            Path::new("/home/me/my \"bin\"/demo"),
            Path::new("/run/user/1000/demo dir/waybar.json"),
            12,
        );
        let snippet = render(WAYBAR_MODULE_TEMPLATE, &vars.json_escaped()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&format!("{{{snippet}}}")).unwrap();
        let module = &parsed[WAYBAR_MODULE];
        assert_eq!(module["signal"], 12);
        assert_eq!(module["return-type"], "json");
        assert_eq!(
            module["exec"],
            "cat \"/run/user/1000/demo dir/waybar.json\""
        );
        assert_eq!(module["on-click"], "\"/home/me/my \\\"bin\\\"/demo\" popup");
    }

    #[test]
    fn shell_quote_only_when_needed() {
        assert_eq!(shell_quote("/usr/bin/x"), "/usr/bin/x");
        assert_eq!(shell_quote("/a b/x"), "\"/a b/x\"");
        assert_eq!(shell_quote("a$b"), "\"a\\$b\"");
    }

    #[test]
    fn capitalize_first_letter() {
        assert_eq!(capitalize("demo"), "Demo");
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn rendered_default_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = render(CONFIG_TEMPLATE, &Vars::for_config(&Config::default())).unwrap();
        std::fs::write(&path, &text).unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(Config::default()).unwrap()
        );
        assert!(
            text.lines().any(|l| l.starts_with('#')),
            "config should be commented"
        );
    }

    #[test]
    fn write_files_never_overwrites_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let t = Targets {
            unit: dir.path().join("config/systemd/user/x.service"),
            desktop: dir.path().join("data/applications/x.desktop"),
            config_dir: dir.path().join("config/x"),
            config_file: dir.path().join("config/x/config.toml"),
        };
        let r = render_all(Path::new("/opt/bin/demo"), Path::new("/tmp/w.json"), 8).unwrap();

        let (written, created) = write_files(&t, &r, true).unwrap();
        assert!(created);
        assert_eq!(written.len(), 3);
        assert_eq!(std::fs::read_to_string(&t.config_file).unwrap(), r.config);

        std::fs::write(&t.config_file, "[waybar]\nsignal = 3\n").unwrap();
        let (written, created) = write_files(&t, &r, false).unwrap();
        assert!(!created);
        assert_eq!(written, vec![t.desktop.clone()]);
        assert_eq!(
            std::fs::read_to_string(&t.config_file).unwrap(),
            "[waybar]\nsignal = 3\n"
        );
    }
}
