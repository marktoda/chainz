//! What the user is looking at and what a key press means. No I/O: a key
//! becomes an [`Action`] for the event loop to carry out, so every binding
//! is tested without a terminal.

use std::cell::Cell;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::data::Refresh;
use crate::model::{ChainState, State};

/// The four tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// Every chain's head and health.
    Chains,
    /// The selected chain's endpoints.
    Endpoints,
    /// Signer balances per chain.
    Balances,
    /// Gateway traffic.
    Traffic,
}

impl Tab {
    /// Every tab, in key order.
    pub const ALL: [Tab; 4] = [Tab::Chains, Tab::Endpoints, Tab::Balances, Tab::Traffic];

    /// The tab's title.
    pub fn title(self) -> &'static str {
        match self {
            Tab::Chains => "Chains",
            Tab::Endpoints => "Endpoints",
            Tab::Balances => "Balances",
            Tab::Traffic => "Traffic",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// A transient line in the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The text.
    pub text: String,
    /// Whether it reports a failure.
    pub error: bool,
    /// When it expires, unix milliseconds.
    pub until_ms: u64,
}

/// What the event loop should do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing beyond a redraw.
    None,
    /// Leave the dashboard.
    Quit,
    /// Wake a worker.
    Refresh(Refresh),
    /// Make `endpoint` the active RPC of `chain`.
    SetActive {
        /// Chain name.
        chain: String,
        /// Endpoint name.
        endpoint: String,
    },
    /// Suspend the dashboard and run `chainz shell <chain>`.
    Shell(String),
    /// Put `text` on the clipboard; `what` names it in the footer.
    Copy {
        /// The clipboard contents.
        text: String,
        /// A short description that is safe to show.
        what: String,
    },
}

/// How long a notice stays up.
const NOTICE_MS: u64 = 4_000;
/// Minimum gap between two refreshes, so a held `r` cannot hammer RPCs.
const REFRESH_GAP_MS: u64 = 1_000;

/// The UI state.
#[derive(Debug)]
pub struct App {
    /// The tab on screen.
    pub tab: Tab,
    /// The selected chain, by name; `None` selects the first visible one.
    pub chain: Option<String>,
    /// The selected endpoint of the selected chain.
    pub endpoint: usize,
    /// The signer shown in the Balances tab.
    pub signer: usize,
    /// The chain filter.
    pub filter: String,
    /// Whether keys currently edit the filter.
    pub filtering: bool,
    /// The footer notice.
    pub notice: Option<Notice>,
    /// First visible row of the chain list and of the endpoint list.
    pub scroll: [Cell<usize>; 2],
    pub(crate) last_refresh_ms: u64,
}

impl Default for App {
    fn default() -> Self {
        App {
            tab: Tab::Chains,
            chain: None,
            endpoint: 0,
            signer: 0,
            filter: String::new(),
            filtering: false,
            notice: None,
            scroll: [Cell::new(0), Cell::new(0)],
            last_refresh_ms: 0,
        }
    }
}

impl App {
    /// The chains the filter lets through, in config order.
    pub fn visible<'a>(&self, state: &'a State) -> Vec<&'a ChainState> {
        let needle = self.filter.to_lowercase();
        state
            .chains
            .iter()
            .filter(|c| {
                needle.is_empty()
                    || c.plan.name.to_lowercase().contains(&needle)
                    || c.plan.id.to_string().contains(&needle)
                    || c.plan
                        .aliases
                        .iter()
                        .any(|a| a.to_lowercase().contains(&needle))
            })
            .collect()
    }

    /// Index of the selected chain among the visible ones.
    pub fn chain_index(&self, state: &State) -> usize {
        let visible = self.visible(state);
        self.chain
            .as_deref()
            .and_then(|name| visible.iter().position(|c| c.plan.name == name))
            .unwrap_or(0)
    }

    /// The selected chain.
    pub fn selected<'a>(&self, state: &'a State) -> Option<&'a ChainState> {
        self.visible(state).get(self.chain_index(state)).copied()
    }

    /// The chain whose endpoints should be probed: the selected one, while
    /// the Endpoints tab is on screen.
    pub fn focus(&self, state: &State) -> Option<String> {
        (self.tab == Tab::Endpoints)
            .then(|| self.selected(state).map(|c| c.plan.name.clone()))
            .flatten()
    }

    /// Show `text` in the footer for a few seconds.
    pub fn notify(&mut self, text: impl Into<String>, error: bool, now_ms: u64) {
        self.notice = Some(Notice {
            text: text.into(),
            error,
            until_ms: now_ms + NOTICE_MS,
        });
    }

    /// Drop an expired notice; true when something changed.
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let expired = self.notice.as_ref().is_some_and(|n| n.until_ms <= now_ms);
        if expired {
            self.notice = None;
        }
        expired
    }

    fn select_chain(&mut self, state: &State, index: usize) {
        let visible = self.visible(state);
        if let Some(chain) = visible.get(index.min(visible.len().saturating_sub(1))) {
            if self.chain.as_deref() != Some(chain.plan.name.as_str()) {
                self.endpoint = active_index(chain);
                self.scroll[1].set(0);
            }
            self.chain = Some(chain.plan.name.clone());
        }
    }

    /// Move the selection of the current tab by `delta` rows.
    fn step(&mut self, state: &State, delta: isize) {
        let moved = |index: usize, len: usize| {
            index
                .saturating_add_signed(delta)
                .min(len.saturating_sub(1))
        };
        match self.tab {
            Tab::Chains | Tab::Balances => {
                let len = self.visible(state).len();
                self.select_chain(state, moved(self.chain_index(state), len));
            }
            Tab::Endpoints => {
                let len = self.selected(state).map_or(0, |c| c.endpoints.len());
                self.endpoint = moved(self.endpoint, len);
            }
            Tab::Traffic => {}
        }
    }

    /// Move sideways: the next signer in Balances, the next chain in
    /// Endpoints.
    fn sidestep(&mut self, state: &State, delta: isize) {
        match self.tab {
            Tab::Balances => {
                let len = state.signers.len().max(1);
                self.signer = (self.signer + len).saturating_add_signed(delta) % len;
            }
            Tab::Endpoints => {
                let len = self.visible(state).len();
                let index = self.chain_index(state).saturating_add_signed(delta);
                self.select_chain(state, index.min(len.saturating_sub(1)));
            }
            Tab::Chains | Tab::Traffic => {}
        }
    }

    fn open(&mut self, tab: Tab, state: &State) {
        if tab == Tab::Endpoints && self.tab != Tab::Endpoints {
            // Pin the selection, so the filter cannot move it underfoot.
            let index = self.chain_index(state);
            self.select_chain(state, index);
        }
        self.tab = tab;
    }

    /// Apply one key press.
    pub fn on_key(&mut self, key: KeyEvent, state: &State, now_ms: u64) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => Action::Quit,
                _ => Action::None,
            };
        }
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    if self.filter.pop().is_none() {
                        self.filtering = false;
                    }
                }
                KeyCode::Down => self.step(state, 1),
                KeyCode::Up => self.step(state, -1),
                KeyCode::Char(c) if !c.is_control() => self.filter.push(c),
                _ => {}
            }
            self.scroll[0].set(0);
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                } else if self.tab != Tab::Chains {
                    self.tab = Tab::Chains;
                }
            }
            KeyCode::Char(c @ '1'..='4') => {
                self.open(Tab::ALL[c as usize - '1' as usize], state);
            }
            KeyCode::Tab => self.open(Tab::ALL[(self.tab.index() + 1) % 4], state),
            KeyCode::BackTab => self.open(Tab::ALL[(self.tab.index() + 3) % 4], state),
            KeyCode::Char('b') => self.open(Tab::Balances, state),
            KeyCode::Char('j') | KeyCode::Down => self.step(state, 1),
            KeyCode::Char('k') | KeyCode::Up => self.step(state, -1),
            KeyCode::PageDown => self.step(state, 10),
            KeyCode::PageUp => self.step(state, -10),
            KeyCode::Char('g') | KeyCode::Home => self.step(state, isize::MIN),
            KeyCode::Char('G') | KeyCode::End => self.step(state, isize::MAX),
            KeyCode::Char('l') | KeyCode::Right => self.sidestep(state, 1),
            KeyCode::Char('h') | KeyCode::Left => self.sidestep(state, -1),
            KeyCode::Char('/') if matches!(self.tab, Tab::Chains | Tab::Balances) => {
                self.filtering = true;
            }
            KeyCode::Enter if self.tab == Tab::Chains => self.open(Tab::Endpoints, state),
            KeyCode::Char('r') => return self.refresh(now_ms),
            KeyCode::Char('s') => return self.set_rpc(state, now_ms),
            KeyCode::Char('e') if self.tab != Tab::Traffic => {
                if let Some(chain) = self.selected(state) {
                    return Action::Shell(chain.plan.name.clone());
                }
            }
            KeyCode::Char('y') => return self.copy(state),
            _ => {}
        }
        Action::None
    }

    fn refresh(&mut self, now_ms: u64) -> Action {
        if now_ms.saturating_sub(self.last_refresh_ms) < REFRESH_GAP_MS {
            return Action::None;
        }
        self.last_refresh_ms = now_ms;
        self.notify("refreshing…", false, now_ms);
        Action::Refresh(match self.tab {
            Tab::Chains => Refresh::Chains,
            Tab::Endpoints => Refresh::Endpoints,
            Tab::Balances => Refresh::Balances,
            Tab::Traffic => Refresh::Traffic,
        })
    }

    fn set_rpc(&mut self, state: &State, now_ms: u64) -> Action {
        let Some(chain) = self.selected(state) else {
            return Action::None;
        };
        match self.tab {
            Tab::Chains => {
                self.open(Tab::Endpoints, state);
                self.notify("pick an endpoint, then press s", false, now_ms);
            }
            Tab::Endpoints => {
                let Some(endpoint) = chain.endpoints.get(self.endpoint) else {
                    return Action::None;
                };
                if endpoint.active {
                    let text = format!("'{}' is already the active RPC", endpoint.name);
                    self.notify(text, false, now_ms);
                } else {
                    return Action::SetActive {
                        chain: chain.plan.name.clone(),
                        endpoint: endpoint.name.clone(),
                    };
                }
            }
            Tab::Balances | Tab::Traffic => {}
        }
        Action::None
    }

    /// What `y` copies: the selected endpoint's URL (already redacted
    /// unless `--reveal`), or the shown signer's address.
    fn copy(&mut self, state: &State) -> Action {
        let chain = self.selected(state);
        let endpoint = match self.tab {
            Tab::Chains => chain.and_then(|c| c.endpoints.iter().find(|e| e.active)),
            Tab::Endpoints => chain.and_then(|c| c.endpoints.get(self.endpoint)),
            Tab::Balances => {
                let signer = state.signers.get(self.signer);
                return match signer.and_then(|s| Some((s, s.address?))) {
                    Some((signer, address)) => Action::Copy {
                        text: address.to_checksum(),
                        what: format!("{}'s address", signer.name),
                    },
                    None => Action::None,
                };
            }
            Tab::Traffic => None,
        };
        match (chain, endpoint) {
            (Some(chain), Some(endpoint)) => Action::Copy {
                text: endpoint.url.clone(),
                what: format!("{}.{} URL", chain.plan.name, endpoint.name),
            },
            _ => Action::None,
        }
    }
}

fn active_index(chain: &ChainState) -> usize {
    chain.endpoints.iter().position(|e| e.active).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::testing::{HOLDER_A, chain, plan};
    use chainz_rpc::HealthStore;

    const NOW: u64 = 1_700_000_000_000;

    fn state() -> State {
        let mut eth = chain(
            "eth",
            1,
            &[
                (
                    "infura",
                    "https://mainnet.infura.io/v3/0123456789abcdef0123456789abcdef",
                ),
                ("llama", "https://eth.llamarpc.com"),
                ("publicnode", "https://ethereum-rpc.publicnode.com"),
            ],
        );
        eth.aliases = vec!["mainnet".into()];
        eth.active = "llama".into();
        let plan = plan(vec![
            eth,
            chain("base", 8453, &[("infura", "https://base.example.org")]),
            chain("arb", 42161, &[("uniswap", "https://arb.example.org")]),
        ]);
        let store = HealthStore::empty();
        State::new(&plan, &store)
    }

    fn press(app: &mut App, state: &State, keys: &str) -> Action {
        let mut last = Action::None;
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                '\x08' => KeyCode::Backspace,
                '\t' => KeyCode::Tab,
                c => KeyCode::Char(c),
            };
            last = app.on_key(KeyEvent::new(code, KeyModifiers::NONE), state, NOW);
        }
        last
    }

    fn selected(app: &App, state: &State) -> String {
        app.selected(state).unwrap().plan.name.clone()
    }

    #[test]
    fn number_keys_and_tab_switch_tabs() {
        let (mut app, state) = (App::default(), state());
        for (key, tab) in [
            ("2", Tab::Endpoints),
            ("3", Tab::Balances),
            ("4", Tab::Traffic),
            ("1", Tab::Chains),
        ] {
            press(&mut app, &state, key);
            assert_eq!(app.tab, tab);
        }
        press(&mut app, &state, "\t\t");
        assert_eq!(app.tab, Tab::Balances);
        app.on_key(
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            &state,
            NOW,
        );
        assert_eq!(app.tab, Tab::Endpoints);
        press(&mut app, &state, "b");
        assert_eq!(app.tab, Tab::Balances);
        // esc goes back to the chains.
        press(&mut app, &state, "\x1b");
        assert_eq!(app.tab, Tab::Chains);
    }

    #[test]
    fn j_and_k_move_and_stop_at_the_ends() {
        let (mut app, state) = (App::default(), state());
        assert_eq!(selected(&app, &state), "eth");
        press(&mut app, &state, "j");
        assert_eq!(selected(&app, &state), "base");
        press(&mut app, &state, "jjjj");
        assert_eq!(selected(&app, &state), "arb");
        press(&mut app, &state, "k");
        assert_eq!(selected(&app, &state), "base");
        press(&mut app, &state, "g");
        assert_eq!(selected(&app, &state), "eth");
        press(&mut app, &state, "G");
        assert_eq!(selected(&app, &state), "arb");
        press(&mut app, &state, "kkkkk");
        assert_eq!(selected(&app, &state), "eth");
        // Arrows too.
        app.on_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &state,
            NOW,
        );
        assert_eq!(selected(&app, &state), "base");
    }

    #[test]
    fn enter_drills_into_the_chain_on_its_active_endpoint() {
        let (mut app, state) = (App::default(), state());
        assert_eq!(
            app.focus(&state),
            None,
            "nothing is probed from the Chains tab"
        );
        press(&mut app, &state, "\n");
        assert_eq!(app.tab, Tab::Endpoints);
        assert_eq!(app.focus(&state), Some("eth".into()));
        assert_eq!(app.endpoint, 1, "llama is active");
        press(&mut app, &state, "j");
        assert_eq!(app.endpoint, 2);
        press(&mut app, &state, "jj");
        assert_eq!(app.endpoint, 2);
        // h/l walk the chains without leaving the tab.
        press(&mut app, &state, "l");
        assert_eq!(app.focus(&state), Some("base".into()));
        assert_eq!(app.endpoint, 0);
        press(&mut app, &state, "h");
        assert_eq!(app.focus(&state), Some("eth".into()));
        press(&mut app, &state, "1");
        assert_eq!(app.focus(&state), None);
    }

    #[test]
    fn slash_filters_chains_by_name_alias_or_id() {
        let (mut app, state) = (App::default(), state());
        press(&mut app, &state, "/");
        assert!(app.filtering);
        // While filtering, letters are text: `q` does not quit.
        assert_eq!(press(&mut app, &state, "q"), Action::None);
        assert_eq!(app.filter, "q");
        press(&mut app, &state, "\x08ar");
        assert_eq!(app.filter, "ar");
        assert_eq!(selected(&app, &state), "arb");
        assert_eq!(app.visible(&state).len(), 1);
        // enter keeps the filter and returns the keys.
        press(&mut app, &state, "\n");
        assert!(!app.filtering);
        assert_eq!(app.filter, "ar");
        // esc clears it before it does anything else.
        press(&mut app, &state, "3\x1b");
        assert_eq!((app.filter.as_str(), app.tab), ("", Tab::Balances));
        for (pattern, name) in [("MAINNET", "eth"), ("8453", "base")] {
            press(&mut app, &state, "/");
            press(&mut app, &state, pattern);
            assert_eq!(selected(&app, &state), name, "{pattern}");
            press(&mut app, &state, "\x1b");
            assert!(!app.filtering && app.filter.is_empty());
        }
        // Backspace on an empty filter leaves filter mode.
        press(&mut app, &state, "/\x08");
        assert!(!app.filtering);
        // The filter is for chain lists only.
        press(&mut app, &state, "4/");
        assert!(!app.filtering);
    }

    #[test]
    fn r_refreshes_the_tab_on_screen_at_most_once_a_second() {
        let (mut app, state) = (App::default(), state());
        assert_eq!(
            press(&mut app, &state, "r"),
            Action::Refresh(Refresh::Chains)
        );
        assert_eq!(app.notice.as_ref().unwrap().text, "refreshing…");
        // A held key repeats: only the first press reaches the network.
        assert_eq!(press(&mut app, &state, "rrrr"), Action::None);
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        for (tab, what) in [
            ("2", Refresh::Endpoints),
            ("3", Refresh::Balances),
            ("4", Refresh::Traffic),
        ] {
            press(&mut app, &state, tab);
            app.last_refresh_ms = 0;
            assert_eq!(app.on_key(key, &state, NOW), Action::Refresh(what));
        }
        assert_eq!(app.on_key(key, &state, NOW + 999), Action::None);
        assert_eq!(
            app.on_key(key, &state, NOW + 1_000),
            Action::Refresh(Refresh::Traffic)
        );
    }

    #[test]
    fn s_sets_the_selected_endpoint_active() {
        let (mut app, state) = (App::default(), state());
        // From the chain list, `s` opens the endpoints to choose from.
        assert_eq!(press(&mut app, &state, "s"), Action::None);
        assert_eq!(app.tab, Tab::Endpoints);
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .contains("pick an endpoint")
        );
        // On the active endpoint there is nothing to write.
        assert_eq!(press(&mut app, &state, "s"), Action::None);
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .contains("already the active RPC")
        );
        assert_eq!(
            press(&mut app, &state, "js"),
            Action::SetActive {
                chain: "eth".into(),
                endpoint: "publicnode".into(),
            }
        );
        // Not from the other tabs.
        assert_eq!(press(&mut app, &state, "3s"), Action::None);
        assert_eq!(press(&mut app, &state, "4s"), Action::None);
    }

    #[test]
    fn e_opens_a_shell_for_the_selected_chain() {
        let (mut app, state) = (App::default(), state());
        assert_eq!(press(&mut app, &state, "je"), Action::Shell("base".into()));
        assert_eq!(press(&mut app, &state, "2e"), Action::Shell("base".into()));
        assert_eq!(press(&mut app, &state, "3e"), Action::Shell("base".into()));
        assert_eq!(press(&mut app, &state, "4e"), Action::None);
    }

    #[test]
    fn y_copies_the_redacted_url_or_the_address() {
        let (mut app, state) = (App::default(), state());
        // Chains: the active endpoint's URL, as the plan presents it.
        let Action::Copy { text, what } = press(&mut app, &state, "y") else {
            panic!("expected a copy");
        };
        assert_eq!(
            (text.as_str(), what.as_str()),
            ("https://eth.llamarpc.com/", "eth.llama URL")
        );
        // Endpoints: the selected one. Its credential never reaches the
        // clipboard, because the plan holds only the redacted form.
        let Action::Copy { text, what } = press(&mut app, &state, "\nky") else {
            panic!("expected a copy");
        };
        assert_eq!(what, "eth.infura URL");
        assert_eq!(text, "https://mainnet.infura.io/v3/[REDACTED]");
        // Balances: the shown signer's checksummed address.
        assert_eq!(
            press(&mut app, &state, "3y"),
            Action::Copy {
                text: HOLDER_A.into(),
                what: "default's address".into(),
            }
        );
        // `l` moves to the next signer; one without an address copies nothing.
        assert!(
            matches!(press(&mut app, &state, "ly"), Action::Copy { what, .. } if what == "funder's address")
        );
        assert_eq!(press(&mut app, &state, "ly"), Action::None);
        press(&mut app, &state, "l");
        assert_eq!(app.signer, 0, "wraps around");
        assert_eq!(press(&mut app, &state, "4y"), Action::None);
    }

    #[test]
    fn q_and_ctrl_c_quit_and_releases_are_ignored() {
        let (mut app, state) = (App::default(), state());
        assert_eq!(press(&mut app, &state, "q"), Action::Quit);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.on_key(ctrl_c, &state, NOW), Action::Quit);
        // Even while typing a filter.
        press(&mut app, &state, "/");
        assert_eq!(app.on_key(ctrl_c, &state, NOW), Action::Quit);
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(app.on_key(release, &state, NOW), Action::None);
    }

    #[test]
    fn an_empty_config_has_nothing_to_act_on() {
        let store = HealthStore::empty();
        let state = State::new(&plan(vec![]), &store);
        let mut app = App::default();
        for keys in ["j", "k", "\n", "s", "e", "y", "G", "l", "2js"] {
            assert_eq!(press(&mut app, &state, keys), Action::None, "{keys:?}");
        }
        assert_eq!(app.focus(&state), None);
    }
}
