//! The terminal and the event loop: read keys, fold worker updates, draw.
//! This is the only module that owns the screen.

use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chainz_core::runtime::Runtime;
use chainz_core::{Chainz, LoadMode, edit};
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
use crate::model::{State, Update, now_ms};
use crate::plan::Plan;
use crate::ui::theme::Theme;
use crate::ui::{self, Ctx};
use crate::{Options, fmt, load_store};

/// How long to wait for a key before looking at worker updates.
const POLL: Duration = Duration::from_millis(50);
/// Redraw at least this often, so ages keep counting.
const TICK: Duration = Duration::from_secs(1);

/// The alternate screen in raw mode; restored on drop and on panic.
struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

impl Screen {
    fn enter() -> Result<Self> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
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
}

impl Dashboard {
    /// Stop the workers and forget what they still had queued.
    fn stop(&mut self) {
        self.engine = None;
        while self.updates.try_recv().is_ok() {}
    }

    /// (Re)start the workers for the config as it is now.
    fn start(&mut self) {
        self.stop();
        self.plan = Arc::new(Plan::build(&self.chainz, self.options.reveal));
        self.state = self.state.rebuilt(&self.plan, &self.shared.store());
        self.engine = Some(Engine::start(self.rt.handle(), &self.shared, &self.plan));
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
            }
        }
    }

    fn set_active(&mut self, chain: &str, endpoint: &str) {
        let edited = self
            .chainz
            .transact(|tx| edit::set_active(&mut tx.doc, chain, endpoint));
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

/// Run the interactive dashboard until the user quits.
pub(crate) fn run(chainz: Chainz, runtime: Runtime, options: Options) -> Result<i32> {
    let theme = Theme::from_env();
    let plan = Arc::new(Plan::build(&chainz, options.reveal));
    let store = load_store();
    let mut state = State::new(&plan, &store);
    let (sender, updates) = std::sync::mpsc::channel();
    let shared = Shared::new(
        store,
        runtime,
        chainz.paths().config.clone(),
        Tuning::default(),
        sender,
    );
    state.gateway = shared.gateway();
    let mut app = App::default();
    app.chain.clone_from(&chainz.config().default);

    // First paint before any worker exists: the frame is on screen while
    // the first requests are still in flight.
    let mut screen = Screen::enter()?;
    screen.draw(Ctx::now(&app, &state, theme))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("chainz-top")
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let mut dash = Dashboard {
        engine: Some(Engine::start(rt.handle(), &shared, &plan)),
        chainz,
        options,
        shared,
        updates,
        rt,
        plan,
        state,
        app,
    };

    let mut drawn = Instant::now();
    let mut dirty = false;
    let mut last_key = Instant::now();
    'ui: loop {
        if event::poll(POLL)? {
            // Take everything that is already waiting, then draw once.
            loop {
                match event::read()? {
                    Event::Key(key) => {
                        last_key = Instant::now();
                        let action = dash.app.on_key(key, &dash.state, now_ms());
                        if !dash.act(&mut screen, action)? {
                            break 'ui;
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
        dash.shared.set_idle(last_key.elapsed() >= idle_after);
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
        if let Some(engine) = &dash.engine {
            engine.focus(dash.app.focus(&dash.state));
        }
        dirty |= dash.app.expire(now_ms());
        if dirty || drawn.elapsed() >= TICK {
            screen.draw(Ctx::now(&dash.app, &dash.state, theme))?;
            drawn = Instant::now();
            dirty = false;
        }
    }
    drop(screen);
    // Do not wait for requests still in flight.
    dash.engine = None;
    dash.rt.shutdown_background();
    Ok(0)
}
