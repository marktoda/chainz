//! The `@token` grammar for `exec` arguments.
//!
//! A token is `@` followed by a name of `[a-z0-9_-]` (matched
//! case-insensitively), with an optional `:chain` qualifier.
//!
//! - **Start:** the `@` must start the argument or follow a character
//!   outside the name alphabet, so `user@host` and `foo@bar.com` never
//!   expand.
//! - **End:** the end of the argument, or any character other than `/` or
//!   `.`. If the name (or qualifier) is followed by `/` or `.`, the whole
//!   thing is literal: `npm i @uniswap/v3-core` stays as written.
//! - **Escape:** `@@` produces a literal `@`.
//!
//! Tokenizing never fails; whether a name is known is decided by the
//! caller (an argument that is entirely one unknown token is an error, an
//! unknown token inside a longer argument passes through unchanged).

/// The token-name alphabet, matched case-insensitively.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// One `@name[:chain]` occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenRef<'a> {
    /// The exact source text, e.g. `@USDC:base`.
    pub raw: &'a str,
    /// The name, lowercased.
    pub name: String,
    /// The chain qualifier as written.
    pub chain: Option<&'a str>,
}

/// A piece of a tokenized argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece<'a> {
    /// Literal text (escapes already applied).
    Text(String),
    /// A token.
    Token(TokenRef<'a>),
}

/// Split `arg` into literal text and tokens.
pub(crate) fn tokenize(arg: &str) -> Vec<Piece<'_>> {
    let b = arg.as_bytes();
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut lit_start = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'@' {
            i += 1;
            continue;
        }
        if b.get(i + 1) == Some(&b'@') {
            text.push_str(&arg[lit_start..i]);
            text.push('@');
            i += 2;
            lit_start = i;
            continue;
        }
        let start_ok = i == 0 || !is_token_byte(b[i - 1]);
        let mut j = i + 1;
        while j < b.len() && is_token_byte(b[j]) {
            j += 1;
        }
        if start_ok && j > i + 1 {
            let mut k = j;
            let mut chain = None;
            if b.get(k) == Some(&b':') {
                let mut m = k + 1;
                while m < b.len() && is_token_byte(b[m]) {
                    m += 1;
                }
                if m > k + 1 {
                    chain = Some(&arg[k + 1..m]);
                    k = m;
                }
            }
            if k == b.len() || (b[k] != b'/' && b[k] != b'.') {
                text.push_str(&arg[lit_start..i]);
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                }
                pieces.push(Piece::Token(TokenRef {
                    raw: &arg[i..k],
                    name: arg[i + 1..j].to_ascii_lowercase(),
                    chain,
                }));
                i = k;
                lit_start = k;
                continue;
            }
        }
        i += 1;
    }
    text.push_str(&arg[lit_start..]);
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    pieces
}

/// The token, when `pieces` (the tokenized `arg`) are exactly one token
/// and nothing else.
pub(crate) fn sole_token<'p, 'a>(pieces: &'p [Piece<'a>], arg: &str) -> Option<&'p TokenRef<'a>> {
    match pieces {
        [Piece::Token(t)] if t.raw.len() == arg.len() => Some(t),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(arg: &str) -> String {
        tokenize(arg)
            .into_iter()
            .map(|p| match p {
                Piece::Text(t) => t,
                Piece::Token(t) => format!(
                    "<{}{}>",
                    t.name,
                    t.chain.map(|c| format!(":{c}")).unwrap_or_default()
                ),
            })
            .collect()
    }

    #[test]
    fn tokens_start_only_at_boundaries() {
        assert_eq!(render("@wallet"), "<wallet>");
        assert_eq!(render("--sender=@wallet"), "--sender=<wallet>");
        assert_eq!(render("user@host"), "user@host");
        assert_eq!(render("foo@bar.com"), "foo@bar.com");
        assert_eq!(render("(@USDC)"), "(<usdc>)");
        assert_eq!(render("@"), "@");
        assert_eq!(render("a @ b"), "a @ b");
    }

    #[test]
    fn slash_and_dot_keep_the_whole_thing_literal() {
        assert_eq!(render("@uniswap/v3-core"), "@uniswap/v3-core");
        assert_eq!(render("@usdc.balanceOf"), "@usdc.balanceOf");
        assert_eq!(render("@usdc:base/x"), "@usdc:base/x");
        assert_eq!(render("@usdc,@weth"), "<usdc>,<weth>");
    }

    #[test]
    fn qualifiers_and_escapes() {
        assert_eq!(render("@usdc:base"), "<usdc:base>");
        assert_eq!(render("@wallet:8453"), "<wallet:8453>");
        assert_eq!(render("@usdc:"), "<usdc>:");
        assert_eq!(render("@@wallet"), "@wallet");
        assert_eq!(render("a@@b"), "a@b");
        assert_eq!(render("@@@wallet"), "@<wallet>");
        assert_eq!(
            render("ünï@wallet"),
            "ünï@wallet".replace("@wallet", "<wallet>")
        );
    }

    fn whole_token(arg: &str) -> Option<TokenRef<'_>> {
        sole_token(&tokenize(arg), arg).cloned()
    }

    #[test]
    fn whole_argument_detection() {
        assert_eq!(whole_token("@udsc").unwrap().name, "udsc");
        assert_eq!(whole_token("@usdc:base").unwrap().chain, Some("base"));
        assert!(whole_token("x@usdc").is_none());
        assert!(whole_token("@usdc,").is_none());
        assert!(whole_token("@@usdc").is_none());
    }
}
