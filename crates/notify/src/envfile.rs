// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The two Telegram keys, read from the instance's `.env` — the only
//! secrets file there is (the house law). Only `TELEGRAM_BOT_TOKEN` and
//! `TELEGRAM_CHAT_ID` are taken; every other line is skipped unparsed, the
//! file's bytes are overwritten before they are dropped, and the token is
//! never printed (`Debug` redacts it). The launchd job runs under
//! `env -i`, so no venue key ever sits in this process's environment.

use std::path::Path;

/// The bot token. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// The secret, for building the request path only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        let mut v = std::mem::take(&mut self.0).into_bytes();
        v.fill(0);
    }
}

/// What `.env` gave us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Telegram {
    /// `TELEGRAM_BOT_TOKEN`.
    pub token: Token,
    /// `TELEGRAM_CHAT_ID` (a number, negative for groups, or `@channel`).
    pub chat_id: String,
}

/// Why the keys could not be read. Never carries a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvErr {
    /// The file could not be read.
    Unreadable(String),
    /// A key is absent or empty.
    Missing(&'static str),
    /// A value has an impossible shape.
    Malformed(&'static str),
}

impl std::fmt::Display for EnvErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvErr::Unreadable(e) => write!(f, ".env unreadable: {e}"),
            EnvErr::Missing(k) => write!(f, ".env has no {k}"),
            EnvErr::Malformed(k) => write!(f, ".env {k} is malformed"),
        }
    }
}

fn value_of(line: &str, key: &str) -> Option<String> {
    let l = line.trim();
    let l = l.strip_prefix("export ").map_or(l, str::trim_start);
    let rest = l.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    let v = if (rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2)
        || (rest.starts_with('\'') && rest.ends_with('\'') && rest.len() >= 2)
    {
        &rest[1..rest.len() - 1]
    } else {
        // An unquoted value ends at a ` #` comment.
        rest.split(" #").next().unwrap_or("").trim()
    };
    Some(v.to_string())
}

/// Extract the two keys from `.env` text. The last assignment wins, as in
/// a shell.
///
/// # Errors
/// [`EnvErr::Missing`] / [`EnvErr::Malformed`].
pub fn parse(text: &str) -> Result<Telegram, EnvErr> {
    let mut token = None;
    let mut chat = None;
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(v) = value_of(line, "TELEGRAM_BOT_TOKEN") {
            token = Some(v);
        } else if let Some(v) = value_of(line, "TELEGRAM_CHAT_ID") {
            chat = Some(v);
        }
    }
    let token = token
        .filter(|t| !t.is_empty())
        .ok_or(EnvErr::Missing("TELEGRAM_BOT_TOKEN"))?;
    // `<digits>:<35-ish url-safe chars>`; never echoed, only shape-checked.
    let shape_ok = token.split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty()
            && id.bytes().all(|c| c.is_ascii_digit())
            && secret.len() >= 20
            && secret
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    });
    if !shape_ok {
        let mut v = token.into_bytes();
        v.fill(0);
        return Err(EnvErr::Malformed("TELEGRAM_BOT_TOKEN"));
    }
    let chat_id = chat
        .filter(|c| !c.is_empty())
        .ok_or(EnvErr::Missing("TELEGRAM_CHAT_ID"))?;
    let chat_ok = chat_id.strip_prefix('@').map_or_else(
        || {
            let d = chat_id.strip_prefix('-').unwrap_or(&chat_id);
            !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit())
        },
        |name| !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
    );
    if !chat_ok {
        return Err(EnvErr::Malformed("TELEGRAM_CHAT_ID"));
    }
    Ok(Telegram {
        token: Token(token),
        chat_id,
    })
}

/// Read and extract; the file's bytes are zeroed before they are dropped.
///
/// # Errors
/// As [`parse`], plus [`EnvErr::Unreadable`].
pub fn read(path: &Path) -> Result<Telegram, EnvErr> {
    let bytes = std::fs::read(path).map_err(|e| EnvErr::Unreadable(e.kind().to_string()))?;
    let out = match std::str::from_utf8(&bytes) {
        Ok(text) => parse(text),
        Err(_) => Err(EnvErr::Unreadable("not UTF-8".to_string())),
    };
    let mut bytes = bytes;
    bytes.fill(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOK: &str = "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw";

    #[test]
    fn takes_only_the_two_keys() {
        let text = format!(
            "# comment\nHYPERLIQUID_AGENT_KEY=0xdead\nexport TELEGRAM_BOT_TOKEN=\"{TOK}\"\nTELEGRAM_CHAT_ID = -1001234567890 # group\n"
        );
        let t = parse(&text).unwrap();
        assert_eq!(t.token.expose(), TOK);
        assert_eq!(t.chat_id, "-1001234567890");
        assert_eq!(format!("{:?}", t.token), "Token(<redacted>)");
        assert!(!format!("{t:?}").contains("AAHd"));
    }

    #[test]
    fn last_assignment_wins_and_channels_parse() {
        let text = format!(
            "TELEGRAM_BOT_TOKEN=x\nTELEGRAM_BOT_TOKEN='{TOK}'\nTELEGRAM_CHAT_ID=@my_channel\n"
        );
        let t = parse(&text).unwrap();
        assert_eq!(t.chat_id, "@my_channel");
    }

    #[test]
    fn missing_and_malformed_never_echo_the_value() {
        assert_eq!(
            parse("TELEGRAM_CHAT_ID=1\n").unwrap_err(),
            EnvErr::Missing("TELEGRAM_BOT_TOKEN")
        );
        assert_eq!(
            parse(&format!("TELEGRAM_BOT_TOKEN={TOK}\n")).unwrap_err(),
            EnvErr::Missing("TELEGRAM_CHAT_ID")
        );
        let e = parse("TELEGRAM_BOT_TOKEN=not-a-token\nTELEGRAM_CHAT_ID=1\n").unwrap_err();
        assert_eq!(e, EnvErr::Malformed("TELEGRAM_BOT_TOKEN"));
        assert!(!e.to_string().contains("not-a-token"));
        let e = parse(&format!(
            "TELEGRAM_BOT_TOKEN={TOK}\nTELEGRAM_CHAT_ID=12ab\n"
        ))
        .unwrap_err();
        assert_eq!(e, EnvErr::Malformed("TELEGRAM_CHAT_ID"));
        // A key that only starts with the name is not the key.
        assert!(parse(&format!(
            "TELEGRAM_BOT_TOKEN_OLD={TOK}\nTELEGRAM_CHAT_ID=1\n"
        ))
        .is_err());
    }
}
