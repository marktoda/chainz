//! Tab 3: one signer's native and token balances on every chain.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::grid::{Cell, Col, Grid};
use super::{Ctx, context_line, empty, scroll_hint, split_context};
use chainz_core::labels::NATIVE_DECIMALS;

use crate::fmt;
use crate::model::Via;

pub(super) fn render(area: Rect, buf: &mut Buffer, ctx: Ctx) {
    let (top, body) = split_context(area);
    let t = ctx.theme;
    let state = ctx.state;
    if state.signers.iter().all(|s| s.address.is_none()) {
        empty(
            body,
            buf,
            vec![
                Line::styled("No signer has a cached address.", t.text()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("Add one with  ", t.dim()),
                    Span::styled("chainz signer add default", t.accent()),
                ]),
                Line::styled("Balances never unlock a key.", t.dim()),
            ],
        );
        return;
    }
    let shown = ctx.app.signer.min(state.signers.len() - 1);
    let signer = &state.signers[shown];

    // The signers as chips, the shown one marked.
    let mut chips = vec![Span::styled("signer ", t.dim())];
    for (i, s) in state.signers.iter().enumerate() {
        let address = match &s.address {
            Some(a) => fmt::short_address(a),
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
    context_line(top, buf, t, vec![chips]);

    if signer.address.is_none() {
        let text = format!("Signer '{}' has no cached address.", signer.name);
        empty(body, buf, vec![Line::styled(text, t.dim())]);
        return;
    }
    let visible = ctx.app.visible(state);
    if visible.is_empty() {
        let text = format!("No chain matches /{}", ctx.app.filter);
        empty(body, buf, vec![Line::styled(text, t.dim())]);
        return;
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

    let amount = |raw: Option<u128>, decimals: u8, symbol: Option<&str>| match raw {
        None => Cell::new("?", t.warn()),
        Some(raw) => {
            let style = if raw == 0 { t.faint() } else { t.text() };
            let cell = Cell::new(fmt::units(raw, decimals), style);
            match symbol {
                Some(symbol) => cell.then(format!(" {symbol}"), t.faint()),
                None => cell,
            }
        }
    };
    for chain in &visible {
        let holding = chain
            .balances
            .as_ref()
            .and_then(|sheet| sheet.holders.iter().find(|h| h.signer == signer.name));
        let pending = || {
            Cell::new(
                if chain.balance_fault.is_some() {
                    "–"
                } else {
                    "…"
                },
                t.faint(),
            )
        };
        let mut row = vec![Cell::new(chain.plan.name.clone(), t.text())];
        row.push(match holding {
            Some(h) => amount(h.native, NATIVE_DECIMALS, Some(chain.plan.native_symbol)),
            None => pending(),
        });
        for name in &state.token_names {
            let index = chain
                .plan
                .tokens
                .iter()
                .position(|token| &token.name == name);
            row.push(match (index, holding) {
                // The token does not exist on this chain.
                (None, _) => Cell::empty(),
                (Some(i), Some(h)) => amount(
                    h.tokens.get(i).copied().flatten(),
                    chain.plan.tokens[i].decimals,
                    None,
                ),
                (Some(_), None) => pending(),
            });
        }
        row.push(match chain.balances.as_ref().map(|sheet| sheet.via) {
            Some(Via::Multicall3) => Cell::new("multicall", t.dim()),
            Some(Via::Batch) => Cell::new("batch", t.dim()),
            None => Cell::empty(),
        });
        row.push(match (&chain.balance_fault, chain.balances_at_ms) {
            (Some(fault), _) => Cell::new(fault.class.to_string(), t.bad()),
            (None, Some(at)) => Cell::new(
                format!("{} ago", fmt::age(ctx.now_ms.saturating_sub(at) / 1_000)),
                t.dim(),
            ),
            (None, None) => Cell::new("…", t.faint()),
        });
        grid.rows.push(row);
    }
    let shown = grid.render(
        body,
        buf,
        t,
        Some(ctx.app.chain_index(state)),
        Some(&ctx.app.scroll[0]),
    );
    scroll_hint(top, buf, t, shown, visible.len());
}
