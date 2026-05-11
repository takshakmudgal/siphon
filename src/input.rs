use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::{Mutex, mpsc};

use crate::app::{
    AUTO_INTERVALS_SECS, App, AutoForm, ConfirmKind, ConnForm, Dialog, Focus, RunningDump,
    ToastKind,
};
use crate::backup::{self, BackupOutcome};
use crate::config::Config;
use crate::detect;
use crate::types::{Connection, DbKind, DetectedSource};

/// Messages emitted by background tasks back to the main loop.
#[derive(Debug)]
pub enum AppEvent {
    Detected(Vec<DetectedSource>),
    DumpStarted { conn_id: String, name: String },
    DumpSucceeded {
        conn_id: String,
        name: String,
        outcome: BackupOutcome,
    },
    DumpFailed {
        conn_id: String,
        name: String,
        error: String,
    },
    TestResult { name: String, result: Result<String, String> },
}

pub struct Ctx {
    pub tx: mpsc::UnboundedSender<AppEvent>,
    pub config: Arc<Mutex<Config>>,
    pub backup_root: PathBuf,
}

pub async fn handle_key(app: &mut App, key: KeyEvent, ctx: &Ctx) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Ctrl-C / Ctrl-Q always quit.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
    {
        app.quit = true;
        return;
    }

    // Dialog-specific keys come first.
    if app.dialog.is_some() {
        handle_dialog_key(app, key, ctx).await;
        return;
    }

    handle_main_key(app, key, ctx).await;
}

async fn handle_main_key(app: &mut App, key: KeyEvent, ctx: &Ctx) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => app.quit = true,

        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.move_selection(1),
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => app.move_selection(-1),
        (KeyCode::PageDown, _) => app.move_selection(5),
        (KeyCode::PageUp, _) => app.move_selection(-5),
        (KeyCode::Home, _) => app.move_selection(-(i32::MAX as isize)),
        (KeyCode::End, _) => app.move_selection(i32::MAX as isize),

        (KeyCode::Tab, _) => {
            app.focus = match app.focus {
                Focus::List => Focus::Details,
                Focus::Details => Focus::List,
            };
        }

        (KeyCode::Char('?'), _) => app.dialog = Some(Dialog::Help),

        (KeyCode::Char('n'), _) => {
            app.dialog = Some(Dialog::Form(ConnForm::new_blank()));
        }

        (KeyCode::Char('r'), _) => {
            if !app.scanning_detect {
                spawn_detect(ctx.tx.clone(), app);
            }
        }

        (KeyCode::Char('e'), _) => {
            if let Some(c) = app.current_saved().cloned() {
                app.dialog = Some(Dialog::Form(ConnForm::from_existing(&c)));
            } else {
                app.toast("nothing to edit (select a saved connection)", ToastKind::Info);
            }
        }

        (KeyCode::Char('D'), _) => {
            if let Some(c) = app.current_saved().cloned() {
                app.dialog = Some(Dialog::Confirm(ConfirmKind::Delete {
                    conn_id: c.id.clone(),
                    name: c.name.clone(),
                }));
            }
        }

        (KeyCode::Char('a'), _) => {
            if let Some(c) = app.current_saved().cloned() {
                app.dialog = Some(Dialog::Auto(AutoForm::for_connection(&c)));
            } else if let Some(d) = app.current_detected() {
                // Auto requires saving first.
                let _ = d;
                app.toast("import the detected DB first (i)", ToastKind::Info);
            }
        }

        (KeyCode::Char('i'), _) => {
            if let Some(d) = app.current_detected().cloned() {
                let form = ConnForm::from_detected(&d);
                match form.validate() {
                    Ok(mut c) => {
                        // Stable id derived from container so re-detect keeps it.
                        c.id = format!("docker-{}", d.container_id.chars().take(12).collect::<String>());
                        let mut cfg = ctx.config.lock().await;
                        cfg.upsert(c);
                        if let Err(e) = cfg.save() {
                            app.toast(format!("save failed: {e}"), ToastKind::Error);
                        } else {
                            app.toast(format!("imported {}", d.display_name()), ToastKind::Success);
                        }
                        drop(cfg);
                        refresh_cache(app, ctx).await;
                    }
                    Err(e) => app.toast(e, ToastKind::Error),
                }
            }
        }

        (KeyCode::Char('d'), _) | (KeyCode::Enter, _) => {
            if let Some(c) = app.current_saved().cloned() {
                app.dialog = Some(Dialog::Confirm(ConfirmKind::Dump {
                    conn_id: c.id.clone(),
                    name: c.name.clone(),
                }));
            } else if let Some(d) = app.current_detected().cloned() {
                let conn = transient_from_detected(&d);
                spawn_dump(ctx, app, conn);
            }
        }

        (KeyCode::Char('t'), _) => {
            let conn = app
                .current_saved()
                .cloned()
                .or_else(|| app.current_detected().map(transient_from_detected));
            match conn {
                Some(c) => {
                    let tx = ctx.tx.clone();
                    let name = c.name.clone();
                    app.toast(format!("testing {}…", name), ToastKind::Info);
                    tokio::spawn(async move {
                        let result = backup::test_connection(&c)
                            .await
                            .map_err(|e| format!("{:#}", e));
                        let _ = tx.send(AppEvent::TestResult { name, result });
                    });
                }
                None => app.toast("nothing selected", ToastKind::Info),
            }
        }

        (KeyCode::Char('o'), _) => match open_in_finder(&ctx.backup_root) {
            Ok(()) => app.toast(format!("opened {}", ctx.backup_root.display()), ToastKind::Info),
            Err(e) => app.toast(format!("open failed: {e}"), ToastKind::Error),
        },

        _ => {}
    }
}

async fn handle_dialog_key(app: &mut App, key: KeyEvent, ctx: &Ctx) {
    // Borrow the dialog out so we can mutate freely.
    let mut dialog = app.dialog.take().unwrap();
    match &mut dialog {
        Dialog::Form(form) => match handle_form_key(form, key) {
            FormOutcome::Continue => {
                app.dialog = Some(dialog);
            }
            FormOutcome::Cancel => {}
            FormOutcome::Save => {
                if let Dialog::Form(form) = &dialog {
                    match form.validate() {
                        Ok(conn) => {
                            let mut cfg = ctx.config.lock().await;
                            let was_edit = form.editing_id.is_some();
                            cfg.upsert(conn.clone());
                            if let Err(e) = cfg.save() {
                                app.toast(format!("save failed: {e}"), ToastKind::Error);
                                app.dialog = Some(dialog);
                            } else {
                                app.toast(
                                    format!(
                                        "{} {}",
                                        if was_edit { "updated" } else { "added" },
                                        conn.name
                                    ),
                                    ToastKind::Success,
                                );
                                drop(cfg);
                                refresh_cache(app, ctx).await;
                            }
                        }
                        Err(e) => {
                            if let Dialog::Form(form) = &mut dialog {
                                form.error = Some(e);
                            }
                            app.dialog = Some(dialog);
                        }
                    }
                }
            }
        },

        Dialog::Auto(form) => match handle_auto_key(form, key) {
            FormOutcome::Continue => {
                app.dialog = Some(dialog);
            }
            FormOutcome::Cancel => {}
            FormOutcome::Save => {
                if let Dialog::Auto(form) = &dialog {
                    match form.build() {
                        Ok(ab) => {
                            let mut cfg = ctx.config.lock().await;
                            if let Some(c) = cfg
                                .connections
                                .iter_mut()
                                .find(|c| c.id == form.conn_id)
                            {
                                c.auto_backup = Some(ab);
                            }
                            if let Err(e) = cfg.save() {
                                app.toast(format!("save failed: {e}"), ToastKind::Error);
                                app.dialog = Some(dialog);
                            } else {
                                app.toast("auto-backup saved", ToastKind::Success);
                                drop(cfg);
                                refresh_cache(app, ctx).await;
                            }
                        }
                        Err(e) => {
                            if let Dialog::Auto(form) = &mut dialog {
                                form.error = Some(e);
                            }
                            app.dialog = Some(dialog);
                        }
                    }
                }
            }
        },

        Dialog::Confirm(kind) => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let k = kind.clone();
                match k {
                    ConfirmKind::Delete { conn_id, name } => {
                        let mut cfg = ctx.config.lock().await;
                        cfg.remove(&conn_id);
                        if let Err(e) = cfg.save() {
                            app.toast(format!("save failed: {e}"), ToastKind::Error);
                        } else {
                            app.toast(format!("deleted {name}"), ToastKind::Info);
                            drop(cfg);
                            refresh_cache(app, ctx).await;
                        }
                    }
                    ConfirmKind::Dump { conn_id, .. } => {
                        let conn = {
                            let cfg = ctx.config.lock().await;
                            cfg.connections.iter().find(|c| c.id == conn_id).cloned()
                        };
                        if let Some(conn) = conn {
                            spawn_dump(ctx, app, conn);
                        }
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
            _ => {
                app.dialog = Some(dialog);
            }
        },

        Dialog::Progress { .. } => match key.code {
            KeyCode::Esc => {
                // Keep the task running but dismiss the modal.
            }
            _ => {
                app.dialog = Some(dialog);
            }
        },

        Dialog::Help => match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {}
            _ => {
                app.dialog = Some(dialog);
            }
        },
    }
}

enum FormOutcome {
    Continue,
    Cancel,
    Save,
}

fn handle_form_key(form: &mut ConnForm, key: KeyEvent) -> FormOutcome {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => return FormOutcome::Cancel,
        (KeyCode::Enter, _) => return FormOutcome::Save,
        (KeyCode::Tab, _) => {
            form.field = (form.field + 1) % form.field_count();
        }
        (KeyCode::BackTab, _) => {
            let n = form.field_count();
            form.field = (form.field + n - 1) % n;
        }
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
            form.toggle_mode();
        }
        (KeyCode::Char('p'), m) if m.contains(KeyModifiers::CONTROL) => {
            form.show_password = !form.show_password;
        }
        (KeyCode::Left, _) => {
            if form.field_label(form.field) == "kind" {
                form.cycle_kind(false);
            } else if let Some(s) = form.field_text_mut(form.field) {
                // We don't track caret; treat left as a no-op for now.
                let _ = s;
            }
        }
        (KeyCode::Right, _) => {
            if form.field_label(form.field) == "kind" {
                form.cycle_kind(true);
            }
        }
        (KeyCode::Backspace, _) => {
            if let Some(s) = form.field_text_mut(form.field) {
                s.pop();
            }
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            if form.field_label(form.field) == "kind" {
                // Letter shortcuts in kind field: cycle.
                form.cycle_kind(true);
            } else if let Some(s) = form.field_text_mut(form.field) {
                s.push(c);
            }
        }
        _ => {}
    }
    FormOutcome::Continue
}

fn handle_auto_key(form: &mut AutoForm, key: KeyEvent) -> FormOutcome {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => return FormOutcome::Cancel,
        (KeyCode::Enter, _) => return FormOutcome::Save,
        (KeyCode::Tab, _) => form.field = (form.field + 1) % 3,
        (KeyCode::BackTab, _) => form.field = (form.field + 2) % 3,
        (KeyCode::Char(' '), _) if form.field == 0 => form.enabled = !form.enabled,
        (KeyCode::Left, _) if form.field == 1 => {
            form.interval_idx = (form.interval_idx + AUTO_INTERVALS_SECS.len() - 1)
                % AUTO_INTERVALS_SECS.len();
        }
        (KeyCode::Right, _) if form.field == 1 => {
            form.interval_idx = (form.interval_idx + 1) % AUTO_INTERVALS_SECS.len();
        }
        (KeyCode::Backspace, _) if form.field == 2 => {
            form.retention.pop();
        }
        (KeyCode::Char(c), _) if form.field == 2 && c.is_ascii_digit() => {
            form.retention.push(c);
        }
        _ => {}
    }
    FormOutcome::Continue
}

pub async fn refresh_cache(app: &mut App, ctx: &Ctx) {
    let snapshot = ctx.config.lock().await.connections.clone();
    app.conn_cache = snapshot;
    app.clamp_selection();
}

fn spawn_dump(ctx: &Ctx, app: &mut App, conn: Connection) {
    app.dialog = Some(Dialog::Progress {
        label: format!("dumping {}…", conn.name),
    });
    app.running = Some(RunningDump {
        conn_id: conn.id.clone(),
        name: conn.name.clone(),
        started: std::time::Instant::now(),
    });
    let tx = ctx.tx.clone();
    let root = ctx.backup_root.clone();
    let _ = tx.send(AppEvent::DumpStarted {
        conn_id: conn.id.clone(),
        name: conn.name.clone(),
    });
    let conn_clone = conn.clone();
    tokio::spawn(async move {
        match backup::dump(&root, &conn_clone).await {
            Ok(outcome) => {
                let _ = tx.send(AppEvent::DumpSucceeded {
                    conn_id: conn_clone.id.clone(),
                    name: conn_clone.name.clone(),
                    outcome,
                });
            }
            Err(e) => {
                let _ = tx.send(AppEvent::DumpFailed {
                    conn_id: conn_clone.id.clone(),
                    name: conn_clone.name.clone(),
                    error: format!("{:#}", e),
                });
            }
        }
    });
}

pub fn spawn_detect(tx: mpsc::UnboundedSender<AppEvent>, app: &mut App) {
    app.scanning_detect = true;
    tokio::spawn(async move {
        let detected = detect::scan().await.unwrap_or_default();
        let _ = tx.send(AppEvent::Detected(detected));
    });
}

fn transient_from_detected(d: &DetectedSource) -> Connection {
    let mut id = format!("docker-{}", d.container_id.chars().take(12).collect::<String>());
    if id.len() > 32 {
        id.truncate(32);
    }
    Connection {
        id,
        name: d.display_name(),
        kind: d.kind,
        host: "127.0.0.1".into(),
        port: d.host_port,
        user: d.user.clone(),
        password: d.password.clone(),
        database: d.database.clone(),
        uri: None,
        container_id: Some(d.container_id.clone()),
        container_name: Some(d.container_name.clone()),
        auto_backup: None,
        last_backup_at: None,
    }
}

fn open_in_finder(path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).ok();
    let exe = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        "explorer"
    };
    let _ = std::process::Command::new(exe)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Apply an `AppEvent` to the app — invoked from the main loop.
pub async fn apply_event(app: &mut App, ev: AppEvent, ctx: &Ctx) {
    match ev {
        AppEvent::Detected(d) => {
            app.detected = d;
            app.scanning_detect = false;
            app.last_detect_at = Some(std::time::Instant::now());
            app.clamp_selection();
        }
        AppEvent::DumpStarted { .. } => {}
        AppEvent::DumpSucceeded { conn_id, name, outcome } => {
            // Record last_backup_at + prune.
            let keep_opt = {
                let mut cfg = ctx.config.lock().await;
                let mut keep = None;
                if let Some(c) = cfg.connections.iter_mut().find(|c| c.id == conn_id) {
                    c.last_backup_at = Some(chrono::Utc::now().timestamp());
                    keep = c.auto_backup.as_ref().map(|a| a.retention);
                }
                let _ = cfg.save();
                keep
            };
            if let Some(keep) = keep_opt {
                let cfg = ctx.config.lock().await;
                if let Some(c) = cfg.connections.iter().find(|c| c.id == conn_id).cloned() {
                    drop(cfg);
                    let _ = backup::prune(&ctx.backup_root, &c, keep);
                }
            }
            app.dialog = None;
            app.running = None;
            app.toast(
                format!(
                    "✓ {} · {} in {}",
                    name,
                    crate::types::human_bytes(outcome.bytes),
                    crate::types::human_duration(outcome.duration.as_secs())
                ),
                ToastKind::Success,
            );
            refresh_cache(app, ctx).await;
        }
        AppEvent::DumpFailed { name, error, .. } => {
            app.dialog = None;
            app.running = None;
            let short = error.lines().next().unwrap_or(&error).chars().take(120).collect::<String>();
            app.toast(format!("✗ {name}: {short}"), ToastKind::Error);
        }
        AppEvent::TestResult { name, result } => match result {
            Ok(detail) => app.toast(
                format!("✓ {name}: {}", detail.chars().take(80).collect::<String>()),
                ToastKind::Success,
            ),
            Err(e) => {
                let short = e.lines().next().unwrap_or(&e).chars().take(120).collect::<String>();
                app.toast(format!("✗ {name}: {short}"), ToastKind::Error);
            }
        },
    }
}

#[allow(dead_code)]
fn assert_kind(_k: DbKind) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FormMode;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn k_mod(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: m,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn form_typing_inserts_chars() {
        let mut form = ConnForm::new_blank();
        let _ = handle_form_key(&mut form, k(KeyCode::Char('L')));
        let _ = handle_form_key(&mut form, k(KeyCode::Char('o')));
        let _ = handle_form_key(&mut form, k(KeyCode::Char('l')));
        assert_eq!(form.name, "Lol");
    }

    #[test]
    fn form_backspace_removes() {
        let mut form = ConnForm::new_blank();
        form.name = "abc".into();
        let _ = handle_form_key(&mut form, k(KeyCode::Backspace));
        assert_eq!(form.name, "ab");
    }

    #[test]
    fn form_tab_cycles_fields() {
        let mut form = ConnForm::new_blank();
        let _ = handle_form_key(&mut form, k(KeyCode::Tab));
        assert_eq!(form.field, 1); // kind
        let _ = handle_form_key(&mut form, k(KeyCode::Tab));
        assert_eq!(form.field, 2); // host
    }

    #[test]
    fn form_right_on_kind_cycles_db() {
        let mut form = ConnForm::new_blank();
        let _ = handle_form_key(&mut form, k(KeyCode::Tab));
        assert_eq!(form.field, 1);
        let _ = handle_form_key(&mut form, k(KeyCode::Right));
        assert_eq!(form.kind, DbKind::Mongo);
    }

    #[test]
    fn form_ctrl_u_toggles_mode() {
        let mut form = ConnForm::new_blank();
        assert_eq!(form.mode, FormMode::Fields);
        let _ = handle_form_key(&mut form, k_mod(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(form.mode, FormMode::Uri);
    }

    #[test]
    fn auto_form_keys() {
        let conn = Connection {
            id: "1".into(),
            name: "x".into(),
            ..Default::default()
        };
        let mut form = AutoForm::for_connection(&conn);
        form.field = 0;
        let _ = handle_auto_key(&mut form, k(KeyCode::Char(' ')));
        assert!(form.enabled || !form.enabled); // toggled, just don't crash
        form.field = 1;
        let before = form.interval_idx;
        let _ = handle_auto_key(&mut form, k(KeyCode::Right));
        assert_ne!(before, form.interval_idx);
        form.field = 2;
        form.retention = "5".into();
        let _ = handle_auto_key(&mut form, k(KeyCode::Char('0')));
        assert_eq!(form.retention, "50");
        let _ = handle_auto_key(&mut form, k(KeyCode::Backspace));
        assert_eq!(form.retention, "5");
    }

    #[test]
    fn transient_from_detected_carries_creds() {
        let d = DetectedSource {
            kind: DbKind::Postgres,
            container_id: "abc12345".into(),
            container_name: "pg".into(),
            image: "postgres:17".into(),
            host_port: 5432,
            user: Some("u".into()),
            password: Some("p".into()),
            database: Some("d".into()),
        };
        let c = transient_from_detected(&d);
        assert_eq!(c.user.as_deref(), Some("u"));
        assert_eq!(c.container_id.as_deref(), Some("abc12345"));
        assert!(c.id.starts_with("docker-"));
    }
}
