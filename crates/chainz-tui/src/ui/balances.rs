//! Tab 3: one signer's native and token balances on every chain.

use chainz_core::fmt as core_fmt;
use ratatui::text::{Line, Span};

use super::grid::{Cell, Col, Grid};
use super::theme::Theme;
use super::{Body, Content, Ctx};
use crate::app::ScrollSlot;
use crate::fmt;
use crate::model::Via;
use crate::view::{self, Balance};

/// One balance cell; the symbol only where `with_symbol`.
fn balance_cell(balance: Balance, with_symbol: bool, t: Theme) -> Cell {
    match balance {
        Balance::Pending => Cell::new("…", t.faint()),
        Balance::Unread => Cell::new("–", t.faint()),
        Balance::Failed => Cell::new("?", t.warn()),
        Balance::Read(amount) => {
            let style = if amount.raw == 0 { t.faint() } else { t.text() };
            let cell = Cell::new(fmt::units(amount.raw, amount.decimals), style);
            if with_symbol {
                cell.then(format!(" {}", amount.symbol), t.faint())
            } else {
                cell
            }
        }
    }
}

/// The Balances tab: the signers as chips, then the shown signer's
/// balances on every visible chain.
pub(crate) fn body(ctx: Ctx) -> Body {
    let t = ctx.theme;
    let state = ctx.state;
    if state.signers.iter().all(|s| s.address.is_none()) {
        return Body {
            context: None,
            content: Content::Message(vec![
                Line::styled("No signer has a cached address.", t.text()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("Add one with  ", t.dim()),
                    Span::styled("chainz signer add default", t.accent()),
                ]),
                Line::styled("Balances never unlock a key.", t.dim()),
            ]),
        };
    }
    let shown = ctx.app.signer.min(state.signers.len() - 1);
    let signer = &state.signers[shown];

    // The signers as chips, the shown one marked.
    let mut chips = vec![Span::styled("signer ", t.dim())];
    for (i, s) in state.signers.iter().enumerate() {
        let address = match &s.address {
            Some(a) => core_fmt::short_address(a),
            None => "no address".to_string(),
        };
        if i == shown {
            chips.push(Span::styled(format!("▸{}", s.name), t.tab_active()));
            chips.push(Span::styled(format!(" {address}"), t.text()));
        } else {
            chips.push(Span::styled(format!(" {} {address}", s.name), t.dim()));
        }
        chips.push(Span::raw("   "));
    }
    let message = |text: String| Body {
        context: Some(vec![chips.clone()]),
        content: Content::Message(vec![Line::styled(text, t.dim())]),
    };

    if signer.address.is_none() {
        return message(format!("Signer '{}' has no cached address.", signer.name));
    }
    let visible = ctx.app.visible(state);
    if visible.is_empty() {
        return message(format!("No chain matches /{}", ctx.app.filter));
    }

    let mut cols = vec![Col::new("CHAIN").max(14), Col::new("NATIVE").right()];
    for (i, name) in state.token_names.iter().enumerate() {
        // Narrowing drops how and when first, then tokens from the right.
        let order = 1 + u8::try_from(i).unwrap_or(u8::MAX).min(u8::MAX - 3);
        cols.push(Col::new(format!("@{name}")).right().drop(order));
    }
    cols.push(Col::new("VIA").drop(u8::MAX));
    cols.push(Col::new("UPDATED").max(16).drop(u8::MAX - 1));
    let mut grid = Grid::new(cols);

    for row in view::balance_rows(signer, visible.iter().copied()) {
        let chain = row.chain;
        let mut cells = vec![
            Cell::new(chain.info.name.clone(), t.text()),
            balance_cell(row.native, true, t),
        ];
        for name in &state.token_names {
            cells.push(match row.token(name) {
                // The token does not exist on this chain.
                None => Cell::empty(),
                Some(balance) => balance_cell(balance, false, t),
            });
        }
        cells.push(match row.via {
            Some(Via::Multicall3) => Cell::new("multicall", t.dim()),
            Some(Via::Batch) => Cell::new("batch", t.dim()),
            None => Cell::empty(),
        });
        cells.push(match (&chain.balance_fault, chain.balances_at_ms) {
            (Some(fault), _) => Cell::new(fault.class.to_string(), t.bad()),
            (None, Some(at)) => Cell::new(
                format!("{} ago", fmt::age(ctx.now_ms.saturating_sub(at) / 1_000)),
                t.dim(),
            ),
            (None, None) => Cell::new("…", t.faint()),
        });
        grid.rows.push(cells);
    }
    Body {
        context: Some(vec![chips]),
        content: Content::Table {
            grid,
            selected: Some(ctx.app.chain_index(state)),
            scroll: ScrollSlot::Chains,
        },
    }
}
