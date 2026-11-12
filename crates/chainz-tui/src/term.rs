//! The terminal and the event loop: read keys, fold worker updates, draw.
//! This is the only module that owns the screen.

use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chainz_core::runtime::Runtime;
use chainz_core::{Chainz, LoadMode};
use chainz_rpc::HealthStore;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{execute, queue};

use crate::app::{Action, App};
use crate::data::{Engine, Shared, Tuning};
use crate::model::{State, Update};
use crate::plan::Plan;
use crate::ui::theme::Theme;
use crate::ui::{self, Ctx};
use crate::{Options, fmt, load_store};
use chainz_sys::now_ms;

/// How long to wait for a key before looking at worker updates.
const POLL: Duration = Duration::from_millis(50);
/// Redraw at least this often, so ages keep counting.
const TICK: Duration = Duration::from_secs(1);

/// The alternate screen in raw mode; restored on drop and on a panic of
/// the UI thread.
struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

/// Whether the dashboard holds the terminal (raw mode, alternate screen).
static ON_SCREEN: AtomicBool = AtomicBool::new(false);

fn restore() {
    ON_SCREEN.store(false, Ordering::SeqCst);
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

/// Whether a panic on `panicking` should give the terminal back: only a
/// panic of the UI thread, and only while the dashboard holds the screen.
/// A worker's panic is caught by its runtime and the UI keeps drawing;
/// restoring under it would leave it drawing on the normal screen.
fn restores_on_panic(ui: ThreadId, panicking: ThreadId, on_screen: bool) -> bool {
    panicking == ui && on_screen
}

impl Screen {
    fn enter() -> Result<Self> {
        let ui = std::thread::current().id();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let on_screen = ON_SCREEN.load(Ordering::SeqCst);
            if restores_on_panic(ui, std::thread::current().id(), on_screen) {
                restore();
            }
            previous(info);
        }));
        Ok(Screen {
            terminal: Self::open()?,
        })
    }

    /// Take the terminal: raw mode, alternate screen, no cursor.
    ///
    /// The alternate screen starts blank and a new `Terminal` starts with
    /// empty buffers, so the first draw paints everything. (ratatui's
    /// `clear()` would ask the terminal for its cursor position and wait
    /// for the answer; nothing here needs that round trip.)
    fn open() -> Result<Terminal<CrosstermBackend<Stdout>>> {
        enable_raw_mode().context("failed to enter raw mode")?;
        ON_SCREEN.store(true, Ordering::SeqCst);
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Terminal::new(CrosstermBackend::new(io::stdout())).context("failed to open the terminal")
    }

    /// Take the terminal back after [`restore`].
    fn resume(&mut self) -> Result<()> {
        self.terminal = Self::open()?;
        Ok(())
    }

    fn draw(&mut self, ctx: Ctx) -> Result<()> {
        // One synchronized update per frame: no tearing on a slow terminal.
        queue!(io::stdout(), BeginSynchronizedUpdate)?;
        self.terminal.draw(|frame| ui::draw(frame, ctx))?;
        execute!(io::stdout(), EndSynchronizedUpdate)?;
        Ok(())
    }

    /// Copy through the terminal with OSC 52: works over SSH, needs no
    /// clipboard tool.
    fn copy(&mut self, text: &str) -> io::Result<()> {
        let mut out = io::stdout();
        write!(out, "\x1b]52;c;{}\x07", fmt::base64(text.as_bytes()))?;
        out.flush()
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}

/// Everything the loop owns.
struct Dashboard {
    chainz: Chainz,
    options: Options,
    shared: Arc<Shared>,
    updates: Receiver<Update>,
    rt: tokio::runtime::Runtime,
    plan: Arc<Plan>,
    state: State,
    app: App,
    engine: Option<Engine>,
    /// The focus last handed to the engine; `None` when it does not know.
    focus_sent: Option<Option<String>>,
    /// The idle flag last handed to the workers.
    idle: bool,
}

impl Dashboard {
    /// The dashboard for `chainz`; its workers start with [`Self::run_engine`].
    fn new(chainz: Chainz, runtime: Runtime, options: Options, store: HealthStore) -> Result<Self> {
        let plan = Arc::new(Plan::build(&chainz, options.reveal));
        let mut state = State::new(&plan, &store);
        let (sender, updates) = std::sync::mpsc::channel();
        let shared = Shared::new(
            store,
            runtime,
            chainz.paths().config().to_path_buf(),
            Tuning::default(),
            sender,
        );
        state.gateway = shared.gateway();
        let mut app = App::default();
        app.chain.clone_from(&chainz.config().default);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("chainz-top")
            .enable_all()
            .build()
            .context("failed to start the async runtime")?;
        Ok(Dashboard {
            engine: None,
            chainz,
            options,
            shared,
            updates,
            rt,
            plan,
            state,
            app,
            focus_sent: None,
            idle: false,
        })
    }

    /// Start the workers on the current plan.
    fn run_engine(&mut self) {
        self.engine = Some(Engine::start(self.rt.handle(), &self.shared, &self.plan));
    }

    /// Tell the workers whether anyone is watching, when that changed.
    fn set_idle(&mut self, idle: bool) {
        if idle != self.idle {
            self.idle = idle;
            self.shared.set_idle(idle);
        }
    }

    /// Hand the engine the chain to probe, when that changed.
    fn sync_focus(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        let focus = self.app.focus(&self.state);
        if self.focus_sent.as_ref() != Some(&focus) {
            engine.focus(focus.clone());
            self.focus_sent = Some(focus);
        }
    }

    /// Stop the workers and forget what they still had queued.
    fn stop(&mut self) {
        self.engine = None;
        self.focus_sent = None;
        while self.updates.try_recv().is_ok() {}
    }

    /// Move the workers to the config as it is now: a running engine
    /// restarts only what changed; a stopped one starts.
    fn start(&mut self) {
        // What is queued may come from a chain's previous endpoint.
        while self.updates.try_recv().is_ok() {}
        self.plan = Arc::new(Plan::build(&self.chainz, self.options.reveal));
        self.state = self.state.rebuilt(&self.plan, &self.shared.store());
        match &mut self.engine {
            Some(engine) => engine.replan(&self.plan),
            None => self.run_engine(),
        }
    }

    /// Re-read the config from disk and restart the workers on it.
    fn reload(&mut self) {
        match Chainz::open_at(self.chainz.paths().clone(), LoadMode::Strict) {
            Ok(chainz) => {
                self.chainz = chainz;
                self.start();
            }
            Err(error) => {
                let text = format!("config not reloaded: {error:#}");
                self.app.notify(text, true, now_ms());
                // Back from the shell nothing runs, and `r` and the config
                // watcher run with the engine: carry on with the config
                // loaded last rather than with nothing.
                if self.engine.is_none() {
                    self.start();
                }
            }
        }
    }

    fn set_active(&mut self, chain: &str, endpoint: &str) {
        let edited = self.chainz.transact(|tx| tx.set_active(chain, endpoint));
        match edited {
            Ok(()) => {
                self.start();
                // The gateway watches the config too; this only makes the
                // switch immediate.
                if self.shared.gateway().is_some() {
                    let link = self.shared.link();
                    self.rt.spawn(async move { link.reload().await });
                }
                let text = format!("'{chain}' now uses '{endpoint}'");
                self.app.notify(text, false, now_ms());
            }
            Err(error) => {
                let text = format!("rpc not changed: {error:#}");
                self.app.notify(text, true, now_ms());
            }
        }
    }

    /// Hand the terminal to `chainz shell <chain>` and take it back.
    fn shell(&mut self, screen: &mut Screen, chain: &str) -> Result<()> {
        let Some(mut shell) = self.options.shell.take() else {
            self.app.notify("no shell available", true, now_ms());
            return Ok(());
        };
        // Nothing polls while the dashboard is not on screen.
        self.stop();
        restore();
        let outcome = {
            // ^C belongs to the shell: while we wait, the signal must not
            // kill the dashboard underneath it. (A caught signal resets to
            // the default in the child; an ignored one would not.)
            let _runtime = self.rt.enter();
            let _held = [
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()),
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit()),
            ];
            shell(chain)
        };
        self.options.shell = Some(shell);
        screen.resume()?;
        // The shell may have changed the config (`chainz rpc use …`).
        self.reload();
        match outcome {
            Ok(()) => self
                .app
                .notify(format!("back from {chain}"), false, now_ms()),
            Err(error) => self
                .app
                .notify(format!("shell failed: {error}"), true, now_ms()),
        }
        Ok(())
    }

    /// Carry out one action; false means quit.
    fn act(&mut self, screen: &mut Screen, action: Action) -> Result<bool> {
        match action {
            Action::None => {}
            Action::Quit => return Ok(false),
            Action::Refresh(what) => {
                if let Some(engine) = &self.engine {
                    engine.refresh(what);
                }
            }
            Action::SetActive { chain, endpoint } => self.set_active(&chain, &endpoint),
            Action::Shell(chain) => self.shell(screen, &chain)?,
            Action::Copy { text, what } => match screen.copy(&text) {
                Ok(()) => self.app.notify(format!("copied {what}"), false, now_ms()),
                Err(error) => self
                    .app
                    .notify(format!("copy failed: {error}"), true, now_ms()),
            },
        }
        Ok(true)
    }
}

/// Run the interactive dashboard until the user quits, or a SIGTERM or
/// SIGHUP asks it to. Returns the exit code.
pub(crate) fn run(chainz: Chainz, runtime: Runtime, options: Options) -> Result<i32> {
    let theme = Theme::from_env();
    let mut dash = Dashboard::new(chainz, runtime, options, load_store())?;
    let quit = quit_on_signals(dash.rt.handle())?;
    // First paint before any worker runs: the frame is on screen while
    // the first requests are still in flight.
    let mut screen = Screen::enter()?;
    screen.draw(Ctx::now(&dash.app, &dash.state, theme))?;
    dash.run_engine();

    let mut drawn = Instant::now();
    let mut dirty = false;
    let mut last_key = Instant::now();
    let code = 'ui: loop {
        let signal = quit.load(Ordering::SeqCst);
        if signal != 0 {
            break 'ui 128 + signal;
        }
        if event::poll(POLL)? {
            // Take everything that is already waiting, then draw once.
            loop {
                match event::read()? {
                    Event::Key(key) => {
                        last_key = Instant::now();
                        let action = dash.app.on_key(key, &dash.state, now_ms());
                        if !dash.act(&mut screen, action)? {
                            break 'ui 0;
                        }
                        dirty = true;
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        let idle_after = dash.shared.tuning().idle_after;
        dash.set_idle(last_key.elapsed() >= idle_after);
        let mut reload = false;
        while let Ok(update) = dash.updates.try_recv() {
            match update {
                Update::ConfigChanged => reload = true,
                update => dash.state.apply(update),
            }
            dirty = true;
        }
        if reload {
            dash.reload();
        }
        // The focus follows the tab, the selection and the chain list:
        // keys and updates, both of which mark the frame dirty.
        if dirty || dash.focus_sent.is_none() {
            dash.sync_focus();
        }
        dirty |= dash.app.expire(now_ms());
        if dirty || drawn.elapsed() >= TICK {
            screen.draw(Ctx::now(&dash.app, &dash.state, theme))?;
            drawn = Instant::now();
            dirty = false;
        }
    };
    drop(screen);
    // Do not wait for requests still in flight.
    dash.engine = None;
    dash.rt.shutdown_background();
    Ok(code)
}

/// The number of the first SIGTERM or SIGHUP received, 0 until then. The
/// event loop checks it between key polls and leaves, so the terminal is
/// restored on the way out instead of left raw on the alternate screen.
fn quit_on_signals(rt: &tokio::runtime::Handle) -> Result<Arc<AtomicI32>> {
    use tokio::signal::unix::{SignalKind, signal};
    let _runtime = rt.enter();
    let mut term = signal(SignalKind::terminate()).context("failed to watch SIGTERM")?;
    let mut hup = signal(SignalKind::hangup()).context("failed to watch SIGHUP")?;
    let quit = Arc::new(AtomicI32::new(0));
    let flag = quit.clone();
    rt.spawn(async move {
        let number = tokio::select! {
            _ = term.recv() => SignalKind::terminate().as_raw_value(),
            _ = hup.recv() => SignalKind::hangup().as_raw_value(),
        };
        flag.store(number, Ordering::SeqCst);
    });
    Ok(quit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_core::store::StorePaths;

    #[test]
    fn only_the_ui_thread_panicking_restores_the_terminal() {
        let ui = std::thread::current().id();
        let worker = std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap();
        assert!(restores_on_panic(ui, ui, true));
        assert!(!restores_on_panic(ui, worker, true), "a tokio worker");
        assert!(!restores_on_panic(ui, ui, false), "already restored");
    }

    fn write_config(paths: &StorePaths, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&paths.dir()).unwrap();
        std::fs::write(&paths.config(), text).unwrap();
        let private = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&paths.config(), private).unwrap();
    }

    #[test]
    fn a_failed_reload_with_nothing_running_restarts_the_workers() {
        let dir = crate::data::testing::short_tempdir();
        let paths = StorePaths::in_dir(dir.path().join("config"));
        write_config(&paths, "version = 2\n");
        let chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        let options = Options {
            once: false,
            json: false,
            reveal: false,
            shell: None,
        };
        let store = HealthStore::load(dir.path()).unwrap();
        let runtime = Runtime::at(dir.path().join("runtime"));
        let mut dash = Dashboard::new(chainz, runtime, options, store).unwrap();
        dash.run_engine();

        // `e`: the workers stop while the shell has the terminal, and the
        // shell leaves a broken config behind.
        dash.stop();
        write_config(&paths, "version = 2\n[chains.eth\n");
        dash.reload();
        assert!(dash.engine.is_some(), "the workers run on the old config");
        let notice = dash.app.notice.as_ref().unwrap();
        assert!(notice.error && notice.text.starts_with("config not reloaded"));
        dash.rt.shutdown_background();
    }

    #[test]
    fn sigterm_and_sighup_ask_the_loop_to_quit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let quit = quit_on_signals(rt.handle()).unwrap();
        assert_eq!(quit.load(Ordering::SeqCst), 0);
        let pid = std::process::id().to_string();
        let sent = std::process::Command::new("kill")
            .args(["-HUP", &pid])
            .status()
            .unwrap();
        assert!(sent.success());
        rt.block_on(async {
            let deadline = Instant::now() + Duration::from_secs(10);
            while quit.load(Ordering::SeqCst) == 0 {
                assert!(Instant::now() < deadline, "no SIGHUP seen");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        assert_eq!(quit.load(Ordering::SeqCst), 1, "SIGHUP");
    }
}
