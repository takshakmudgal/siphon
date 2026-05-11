use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CEvent, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{Mutex, mpsc};

use siphon::app::{App, ToastKind};
use siphon::config::Config;
use siphon::input::{self, AppEvent, Ctx};
use siphon::schedule::{Scheduler, SchedulerEvent};
use siphon::ui;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("siphon {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("siphon — dump any db, anytime\n\nUsage: siphon\n\nLaunches an interactive TUI. Config: ~/.siphon/config.toml.\nDocs: https://github.com/takshakmudgal/siphon");
        return Ok(());
    }

    let cfg = Config::load().context("load config")?;
    let backup_root = cfg.effective_backup_dir()?;
    std::fs::create_dir_all(&backup_root).ok();

    let config = Arc::new(Mutex::new(cfg));
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    let (sched_tx, mut sched_rx) = mpsc::unbounded_channel::<SchedulerEvent>();

    let ctx = Ctx {
        tx: tx.clone(),
        config: config.clone(),
        backup_root: backup_root.clone(),
    };

    // Start scheduler.
    let _sched_handle = Scheduler::new(config.clone(), sched_tx).spawn();

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    let mut app = App::new(config.clone(), backup_root.clone());
    input::refresh_cache(&mut app, &ctx).await;
    input::spawn_detect(tx.clone(), &mut app);

    let res = run_app(&mut terminal, &mut app, &ctx, &mut rx, &mut sched_rx).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    if let Err(e) = res {
        eprintln!("siphon error: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    ctx: &Ctx,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    sched_rx: &mut mpsc::UnboundedReceiver<SchedulerEvent>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        app.clear_expired_toast(Duration::from_secs(5));

        tokio::select! {
            term = events.next() => {
                match term {
                    Some(Ok(CEvent::Key(k))) => input::handle_key(app, k, ctx).await,
                    Some(Ok(CEvent::Resize(_, _))) => {}
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            Some(ev) = rx.recv() => input::apply_event(app, ev, ctx).await,
            Some(sev) = sched_rx.recv() => {
                match sev {
                    SchedulerEvent::Started { name, .. } => {
                        app.toast(format!("auto-backup → {name}"), ToastKind::Info);
                    }
                    SchedulerEvent::Succeeded { name, bytes, .. } => {
                        app.toast(
                            format!("auto ✓ {name} · {}", siphon::types::human_bytes(bytes)),
                            ToastKind::Success,
                        );
                        input::refresh_cache(app, ctx).await;
                    }
                    SchedulerEvent::Failed { name, error, .. } => {
                        let short = error.lines().next().unwrap_or(&error)
                            .chars().take(120).collect::<String>();
                        app.toast(format!("auto ✗ {name}: {short}"), ToastKind::Error);
                    }
                }
            }
            _ = tick.tick() => {}
        }

        if app.quit {
            break;
        }
    }
    Ok(())
}
