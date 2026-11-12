//! Address-book queries on top of [`LabelIndex`]: every label for an
//! address across all chains ([`find`]), and the reverse-label text filter
//! behind `chainz label` ([`Labeler`]).

use crate::address::Address;
use crate::labels::{Label, LabelIndex, LabelScope, LabelSource};
use crate::model::Config;
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};

/// Every label naming `address`, on any chain and from any source: user
/// labels, signers, then built-in tokens, each sorted by name and chain ID.
/// Shadowed built-in tokens are included; compare against
/// [`LabelIndex::resolve`] to tell.
pub fn find(config: &Config, address: &Address) -> Vec<Label> {
    let mut out: Vec<Label> = LabelIndex::new(config)
        .labels()
        .filter(|l| l.address == *address)
        .collect();
    out.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    out
}

fn sort_key(label: &Label) -> (LabelSource, &str, Option<u64>) {
    let chain = match label.scope {
        LabelScope::AllChains => None,
        LabelScope::Chain(id) => Some(id),
    };
    (label.source, &label.name, chain)
}

#[derive(Debug)]
enum Mode {
    /// Labels as they resolve on one chain.
    Chain(u64),
    /// No chain context: only names that cannot be wrong on any chain.
    AnyChain(HashMap<Address, String>),
}

/// Rewrites 20-byte hex addresses in text to `name(0xChecksummedAddress)`.
///
/// An address is `0x` followed by exactly 40 hex digits (any case, the
/// checksum is not checked): the digits must not continue into a longer
/// hex run, so 32-byte words and transaction hashes are left alone.
/// Addresses without a label, and everything else (including bytes that
/// are not UTF-8), pass through unchanged.
#[derive(Debug)]
pub struct Labeler {
    index: LabelIndex,
    mode: Mode,
}

/// Longest input kept in memory while waiting for a line break.
const MAX_LINE: usize = 64 * 1024;
/// `0x` plus 40 hex digits.
const ADDRESS_LEN: usize = 42;
/// How much already-written input is kept to recognize `name(` in front
/// of an address at the start of a piece.
const CONTEXT_LEN: usize = 256;

impl Labeler {
    /// Labels as they resolve on `chain_id`: user labels for the chain,
    /// user `"*"` labels, signers and the chain's built-in tokens.
    pub fn for_chain(config: &Config, chain_id: u64) -> Self {
        Labeler {
            index: LabelIndex::new(config),
            mode: Mode::Chain(chain_id),
        }
    }

    /// Labels that hold without knowing the chain: user `"*"` labels and
    /// signers, plus any address whose per-chain labels (user or built-in)
    /// all agree on one name across every chain.
    pub fn any_chain(config: &Config) -> Self {
        let index = LabelIndex::new(config);
        let mut labels: Vec<Label> = index.labels().collect();
        labels.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        let mut everywhere: HashMap<Address, String> = HashMap::new();
        let mut per_chain: HashMap<Address, Option<&str>> = HashMap::new();
        for label in &labels {
            match label.scope {
                LabelScope::AllChains => {
                    everywhere
                        .entry(label.address)
                        .or_insert_with(|| label.name.clone());
                }
                LabelScope::Chain(id) => {
                    let Some(name) = index.reverse(id, &label.address).map(|l| l.name.as_str())
                    else {
                        continue;
                    };
                    per_chain
                        .entry(label.address)
                        .and_modify(|seen| {
                            if *seen != Some(name) {
                                *seen = None;
                            }
                        })
                        .or_insert(Some(name));
                }
            }
        }
        for (address, name) in per_chain {
            if let Some(name) = name {
                everywhere
                    .entry(address)
                    .or_insert_with(|| name.to_string());
            }
        }
        Labeler {
            index,
            mode: Mode::AnyChain(everywhere),
        }
    }

    /// The label this filter gives `address`, if any.
    pub fn name(&self, address: &Address) -> Option<&str> {
        match &self.mode {
            Mode::Chain(id) => self.index.reverse(*id, address).map(|l| l.name.as_str()),
            Mode::AnyChain(names) => names.get(address).map(String::as_str),
        }
    }

    /// Rewrite one line (or any chunk of text) into `out`.
    #[cfg(test)]
    fn rewrite(&self, text: &[u8], out: &mut Vec<u8>) {
        self.rewrite_after(&[], text, out);
    }

    /// [`Self::rewrite`] for text that follows `context` (the tail of what
    /// came before it).
    fn rewrite_after(&self, context: &[u8], text: &[u8], out: &mut Vec<u8>) {
        let mut copied = 0;
        let mut at = 0;
        while let Some(found) = find_0x(&text[at..]) {
            let start = at + found;
            let end = start + ADDRESS_LEN;
            at = start + 2;
            let before = match start {
                0 => context.last().copied(),
                _ => Some(text[start - 1]),
            };
            if before.is_some_and(|b| b.is_ascii_hexdigit())
                || text.len() < end
                || text.get(end).is_some_and(|b| is_word_byte(*b))
            {
                continue;
            }
            let mut bytes = [0u8; 20];
            if hex::decode_to_slice(&text[start + 2..end], &mut bytes).is_err() {
                continue;
            }
            at = end;
            let address = Address::from_bytes(bytes);
            let Some(name) = self.name(&address) else {
                continue;
            };
            // Already labelled (`name(0x…)`): leave it, so the filter can
            // be applied twice.
            if text.get(end) == Some(&b')') && ends_with_call(context, &text[..start], name) {
                continue;
            }
            out.extend_from_slice(&text[copied..start]);
            out.extend_from_slice(name.as_bytes());
            out.push(b'(');
            out.extend_from_slice(address.to_checksum().as_bytes());
            out.push(b')');
            copied = end;
        }
        out.extend_from_slice(&text[copied..]);
    }

    /// Copy `input` to `output` line by line, rewriting addresses and
    /// flushing after every line, so the filter can sit in a live pipe.
    pub fn filter(&self, input: impl BufRead, output: impl Write) -> io::Result<()> {
        self.filter_bounded(input, output, MAX_LINE)
    }

    /// [`Self::filter`], holding at most about `max` bytes while waiting
    /// for a line break. Longer runs (binary input) are processed in
    /// pieces, cut where no address can straddle the cut.
    fn filter_bounded(
        &self,
        mut input: impl BufRead,
        mut output: impl Write,
        max: usize,
    ) -> io::Result<()> {
        let max = max.max(2 * ADDRESS_LEN);
        let mut pending: Vec<u8> = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        let mut context: Vec<u8> = Vec::new();
        loop {
            let room = (max - pending.len().min(max - 1)) as u64;
            let read = (&mut input).take(room).read_until(b'\n', &mut pending)?;
            let eof = read == 0;
            let line = eof || pending.last() == Some(&b'\n');
            if !line && pending.len() < max {
                continue; // the input ended mid-line; the next read says so
            }
            // Where to cut, and whether the piece can hold an address.
            let (cut, scan) = if line {
                (pending.len(), true)
            } else {
                match pending.iter().rposition(|b| !is_hex_or_x(*b)) {
                    // After a byte no address can contain.
                    Some(i) => (i + 1, true),
                    // One long hex run: only its tail could still become
                    // an address, and nothing before the tail can be one.
                    None => (pending.len() - (ADDRESS_LEN + 1), false),
                }
            };
            out.clear();
            if scan {
                self.rewrite_after(&context, &pending[..cut], &mut out);
            } else {
                out.extend_from_slice(&pending[..cut]);
            }
            output.write_all(&out)?;
            if line {
                output.flush()?;
            }
            context.extend(pending.drain(..cut));
            let stale = context.len().saturating_sub(CONTEXT_LEN);
            context.drain(..stale);
            if eof {
                return output.flush();
            }
        }
    }
}

/// Whether `context` followed by `head` ends with `name(`, in any case.
fn ends_with_call(context: &[u8], head: &[u8], name: &str) -> bool {
    let want = name.len() + 1;
    let mut tail = Vec::with_capacity(want);
    let from_head = head.len().min(want);
    let from_context = (want - from_head).min(context.len());
    tail.extend_from_slice(&context[context.len() - from_context..]);
    tail.extend_from_slice(&head[head.len() - from_head..]);
    tail.len() == want
        && tail[want - 1] == b'('
        && tail[..want - 1].eq_ignore_ascii_case(name.as_bytes())
}

fn find_0x(text: &[u8]) -> Option<usize> {
    text.windows(2).position(|w| w == b"0x")
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_hex_or_x(b: u8) -> bool {
    b.is_ascii_hexdigit() || b == b'x' || b == b'X'
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
    const TWO: &str = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF";
    const SAFE: &str = "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69";
    const UNKNOWN: &str = "0x1efF47bc3a10a45D4B230B5d10E37751FE6AA718";
    const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
    const USDC_ETH: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const WETH_OP: &str = "0x4200000000000000000000000000000000000006";

    fn config() -> Config {
        crate::model::Config::parse(&format!(
            r#"
version = 2
[chains.base]
id = 8453
rpc = "a"
rpcs.a = "https://a"
[chains.eth]
id = 1
rpc = "a"
rpcs.a = "https://a"
[signers.funder]
backend = "keyring"
address = "{TWO}"
[addresses.myrouter]
base = "{ONE}"
[addresses.pool]
eth = "{ONE}"
[addresses.my-safe]
"*" = "{SAFE}"
[addresses.usdc]
eth = "{ONE}"
[addresses.stable]
base = "{USDC_BASE}"
"#
        ))
        .unwrap()
    }

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn rewrite(labeler: &Labeler, text: &str) -> String {
        let mut out = Vec::new();
        labeler.rewrite(text.as_bytes(), &mut out);
        String::from_utf8(out).unwrap()
    }

    fn base() -> Labeler {
        Labeler::for_chain(&config(), 8453)
    }

    #[test]
    fn registry_is_well_formed() {
        use crate::labels::{builtin_token, builtin_tokens};
        const NAMES: [&str; 6] = ["weth", "usdc", "usdce", "usdt", "dai", "wbtc"];
        let tokens = builtin_tokens();
        let mut seen = std::collections::HashSet::new();
        for t in tokens {
            let at = format!("{}.{}", t.chain_id, t.name);
            assert!(NAMES.contains(&t.name.as_str()), "{at}: unexpected name");
            assert!(
                crate::names::short_name(t.chain_id).is_some(),
                "{at}: unknown chain"
            );
            assert!(t.source.starts_with("https://"), "{at}: source");
            assert!(!t.symbol.is_empty() && t.address != Address::ZERO, "{at}");
            // One address, one name per chain: reverse labels are unambiguous.
            assert!(
                seen.insert((t.chain_id, t.address)),
                "{at}: duplicate address"
            );
            // The file holds checksummed addresses.
            let line = format!("address = \"{}\"", t.address.to_checksum());
            assert!(
                crate::labels::TOKENS_TOML.contains(&line),
                "{at}: not checksummed"
            );
        }
        // What the chains really say, where it differs from the usual.
        for (chain, name, symbol, decimals) in [
            (1, "usdt", "USDT", 6),
            (1, "dai", "DAI", 18),
            (1, "wbtc", "WBTC", 8),
            (56, "usdc", "USDC", 18),
            (56, "usdt", "USDT", 18),
            (8453, "usdce", "USDbC", 6),
            (42161, "usdt", "USD₮0", 6),
            (43114, "usdce", "USDC.e", 6),
        ] {
            let t = builtin_token(chain, name).unwrap_or_else(|| panic!("{chain}.{name}"));
            assert_eq!(
                (t.symbol.as_str(), t.decimals),
                (symbol, decimals),
                "{chain}.{name}"
            );
        }
    }

    #[test]
    fn find_lists_every_label_across_chains_with_its_source() {
        let config = config();
        let found = find(&config, &addr(ONE));
        let got: Vec<(&str, LabelSource, LabelScope)> = found
            .iter()
            .map(|l| (l.name.as_str(), l.source, l.scope))
            .collect();
        assert_eq!(
            got,
            vec![
                ("myrouter", LabelSource::User, LabelScope::Chain(8453)),
                ("pool", LabelSource::User, LabelScope::Chain(1)),
                ("usdc", LabelSource::User, LabelScope::Chain(1)),
            ]
        );
        // User label first, then the built-in it sits beside.
        let found = find(&config, &addr(USDC_BASE));
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].name.as_str(), found[0].source),
            ("stable", LabelSource::User)
        );
        assert_eq!(
            (found[1].name.as_str(), found[1].source),
            ("usdc", LabelSource::Token)
        );
        assert_eq!(found[1].scope, LabelScope::Chain(8453));

        let found = find(&config, &addr(TWO));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, LabelSource::Signer);
        assert_eq!(found[0].scope, LabelScope::AllChains);
        assert_eq!(find(&config, &addr(SAFE))[0].scope, LabelScope::AllChains);

        // One address, one name, many chains: the OP-stack WETH predeploy.
        let weth = find(&config, &addr(WETH_OP));
        assert!(weth.len() >= 5, "{weth:?}");
        assert!(
            weth.iter()
                .all(|l| l.name == "weth" && l.source == LabelSource::Token)
        );
        assert!(weth.windows(2).all(|w| sort_key(&w[0]) < sort_key(&w[1])));

        assert!(find(&config, &addr(UNKNOWN)).is_empty());
        // Case does not matter.
        assert_eq!(find(&config, &addr(&ONE.to_ascii_lowercase())).len(), 3);
    }

    #[test]
    fn labels_addresses_in_mixed_text() {
        let l = base();
        assert_eq!(
            rewrite(
                &l,
                &format!("from={TWO} to={ONE}, value 5; token {USDC_BASE}.")
            ),
            format!("from=funder({TWO}) to=myrouter({ONE}), value 5; token stable({USDC_BASE}).")
        );
        // Unknown addresses and plain text are untouched.
        let plain = format!("nothing here {UNKNOWN} 0x 0x12 @usdc");
        assert_eq!(rewrite(&l, &plain), plain);
        assert_eq!(rewrite(&l, ""), "");
        // The whole line, the start and the end of a line.
        assert_eq!(rewrite(&l, ONE), format!("myrouter({ONE})"));
        assert_eq!(
            rewrite(&l, &format!("{ONE}\n")),
            format!("myrouter({ONE})\n")
        );
        // JSON, brackets, ANSI colour codes.
        assert_eq!(
            rewrite(
                &l,
                &format!("{{\"to\":\"{ONE}\"}} [{ONE}] \x1b[32m{ONE}\x1b[0m")
            ),
            format!(
                "{{\"to\":\"myrouter({ONE})\"}} [myrouter({ONE})] \x1b[32mmyrouter({ONE})\x1b[0m"
            )
        );
        // Adjacent addresses.
        assert_eq!(
            rewrite(&l, &format!("{ONE},{TWO}:{ONE}")),
            format!("myrouter({ONE}),funder({TWO}):myrouter({ONE})")
        );
    }

    #[test]
    fn labels_respect_the_chain() {
        let config = config();
        let eth = Labeler::for_chain(&config, 1);
        // ONE is `myrouter` on base, `pool` (and a shadowing `usdc`) on eth.
        assert_eq!(rewrite(&base(), ONE), format!("myrouter({ONE})"));
        assert_eq!(rewrite(&eth, ONE), format!("pool({ONE})"));
        // "*" labels and signers apply everywhere.
        for l in [&base(), &eth, &Labeler::for_chain(&config, 999)] {
            assert_eq!(rewrite(l, SAFE), format!("my-safe({SAFE})"));
            assert_eq!(rewrite(l, TWO), format!("funder({TWO})"));
        }
        // Built-in tokens label on their own chain only.
        assert_eq!(
            rewrite(&Labeler::for_chain(&config, 10), WETH_OP),
            format!("weth({WETH_OP})")
        );
        assert_eq!(rewrite(&eth, USDC_BASE), USDC_BASE);
        // The real USDC on eth is shadowed by the user's @usdc, so the name
        // would paste back as another address: it is not used.
        assert_eq!(rewrite(&eth, USDC_ETH), USDC_ETH);
    }

    #[test]
    fn longer_hex_runs_are_not_addresses() {
        let l = base();
        let body = &ONE[2..];
        for text in [
            // 32-byte words, hashes and calldata that contain the address.
            format!("0x000000000000000000000000{body}"),
            format!("0x{body}000000000000000000000000"),
            format!("0x{body}0"),
            format!("0xa9059cbb000000000000000000000000{body}"),
            // 41 and 39 digits.
            format!("0x{body}f rest"),
            format!("0x{} rest", &body[..39]),
            // Glued to the end of a hex digit or into a word.
            format!("ab{ONE}"),
            format!("0{ONE}"),
            format!("{ONE}xyz"),
            format!("{ONE}_1"),
            // No prefix, or the wrong one.
            body.to_string(),
            format!("0X{body}"),
            format!("x{body}"),
        ] {
            assert_eq!(rewrite(&l, &text), text, "{text}");
        }
        // Boundaries that do end an address.
        for (before, after) in [
            ("", ""),
            (" ", " "),
            ("(", ")"),
            ("=", ","),
            ("\"", "\""),
            ("to:", "."),
        ] {
            assert_eq!(
                rewrite(&l, &format!("{before}{ONE}{after}")),
                format!("{before}myrouter({ONE}){after}")
            );
        }
        // A rejected candidate does not hide a real one later in the line.
        assert_eq!(
            rewrite(&l, &format!("0x{body}00 0x0x {ONE}")),
            format!("0x{body}00 0x0x myrouter({ONE})")
        );
    }

    #[test]
    fn any_case_in_and_checksum_out() {
        let l = base();
        let lower = ONE.to_ascii_lowercase();
        let upper = format!("0x{}", ONE[2..].to_ascii_uppercase());
        // A mixed-case form with a wrong checksum is still the same 20 bytes.
        let wrong = format!(
            "0x{}{}",
            ONE[2..22].to_ascii_lowercase(),
            ONE[22..].to_ascii_uppercase()
        );
        assert_ne!(wrong, ONE);
        for text in [ONE, &lower, &upper, &wrong] {
            assert_eq!(rewrite(&l, text), format!("myrouter({ONE})"), "{text}");
        }
        // Unlabelled addresses keep the case they came in.
        let unknown = UNKNOWN.to_ascii_lowercase();
        assert_eq!(rewrite(&l, &unknown), unknown);
    }

    #[test]
    fn already_labelled_addresses_are_left_alone() {
        let l = base();
        let once = rewrite(&l, &format!("a {ONE} b {TWO}"));
        assert_eq!(rewrite(&l, &once), once);
        // Someone else's wrapper is not ours.
        assert_eq!(
            rewrite(&l, &format!("other({ONE})")),
            format!("other(myrouter({ONE}))")
        );
    }

    #[test]
    fn without_a_chain_only_unambiguous_names_are_used() {
        let l = Labeler::any_chain(&config());
        // "*" labels and signers.
        assert_eq!(rewrite(&l, SAFE), format!("my-safe({SAFE})"));
        assert_eq!(rewrite(&l, TWO), format!("funder({TWO})"));
        // ONE is `myrouter` on base but `pool` on eth: ambiguous.
        assert_eq!(rewrite(&l, ONE), ONE);
        // USDC on base has one name there (the user's) and exists nowhere else.
        assert_eq!(rewrite(&l, USDC_BASE), format!("stable({USDC_BASE})"));
        // The same name on every chain that has the address.
        assert_eq!(rewrite(&l, WETH_OP), format!("weth({WETH_OP})"));
        // Shadowed on its only chain: no name.
        assert_eq!(rewrite(&l, USDC_ETH), USDC_ETH);
        assert_eq!(rewrite(&l, UNKNOWN), UNKNOWN);
    }

    #[test]
    fn non_utf8_input_passes_through() {
        let l = base();
        let mut input = vec![0xff, 0xfe, b' '];
        input.extend_from_slice(ONE.as_bytes());
        input.extend_from_slice(&[b' ', 0x80, 0x00, b'\n', 0xc3]);
        let mut out = Vec::new();
        l.filter(&input[..], &mut out).unwrap();
        let mut want = vec![0xff, 0xfe, b' '];
        want.extend_from_slice(format!("myrouter({ONE})").as_bytes());
        want.extend_from_slice(&[b' ', 0x80, 0x00, b'\n', 0xc3]);
        assert_eq!(out, want);
    }

    /// Records each write and flush, to observe streaming.
    #[derive(Default)]
    struct Recorder {
        events: Vec<String>,
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.events.push(String::from_utf8_lossy(buf).into_owned());
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.events.push("<flush>".to_string());
            Ok(())
        }
    }

    #[test]
    fn filter_streams_and_flushes_line_by_line() {
        let l = base();
        let input = format!("first {ONE}\n\nsecond\r\nlast {TWO}");
        let mut rec = Recorder::default();
        // A tiny reader buffer: lines arrive in pieces.
        l.filter(io::BufReader::with_capacity(7, input.as_bytes()), &mut rec)
            .unwrap();
        assert_eq!(
            rec.events,
            vec![
                format!("first myrouter({ONE})\n"),
                "<flush>".to_string(),
                "\n".to_string(),
                "<flush>".to_string(),
                "second\r\n".to_string(),
                "<flush>".to_string(),
                format!("last funder({TWO})"),
                "<flush>".to_string(),
                "<flush>".to_string(),
            ]
        );
        let mut out = Vec::new();
        l.filter(&b""[..], &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn long_lines_are_processed_in_bounded_pieces() {
        let l = base();
        let run = |input: &[u8], max: usize| {
            let mut out = Vec::new();
            l.filter_bounded(input, &mut out, max).unwrap();
            String::from_utf8(out).unwrap()
        };
        // Addresses at every offset relative to the piece boundary.
        for pad in 1..200 {
            let (a, b) = ("-".repeat(pad), ".".repeat(pad));
            let input = format!("{a}{ONE} mid {TWO}{b}{ONE}");
            let want = format!("{a}myrouter({ONE}) mid funder({TWO}){b}myrouter({ONE})");
            assert_eq!(run(input.as_bytes(), 96), want, "pad {pad}");
            assert_eq!(run(input.as_bytes(), MAX_LINE), want, "pad {pad}");
            // Already-labelled text stays as it is, wherever the cut falls.
            assert_eq!(run(want.as_bytes(), 96), want, "pad {pad}");
        }
        // One giant hex run with an address-shaped tail is not an address…
        let blob = format!("0x{}{}", "ab".repeat(300), &ONE[2..]);
        assert_eq!(run(blob.as_bytes(), 96), blob);
        // …and an address after a long hex run still is, across pieces.
        for pad in 0..120 {
            let input = format!("{} {ONE}\n", "f".repeat(pad));
            let want = format!("{} myrouter({ONE})\n", "f".repeat(pad));
            assert_eq!(run(input.as_bytes(), 96), want, "pad {pad}");
            let glued = format!("{}{ONE}\n", "f".repeat(pad + 1));
            assert_eq!(run(glued.as_bytes(), 96), glued, "pad {pad}");
        }
    }
}
