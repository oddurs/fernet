//! Pointer tokens: the components after an open lens.
//!
//! Tokens follow RFC 6901: `~1` is `/` and `~0` is `~`. RFC 6901 allows `~` only
//! before `0` or `1`, which leaves room for a small extension covering keys no
//! filesystem can name (`docs/DECISIONS.md`, entry 9):
//!
//! | token | key |
//! | --- | --- |
//! | `~e` | the empty key |
//! | `~.` | `.` |
//! | `~..` | `..` |
//!
//! The extension forms are whole tokens only.

/// Decodes one token to its key. Returns `None` for an invalid `~` escape.
#[must_use]
pub fn decode_token(token: &str) -> Option<String> {
    match token {
        "~e" => return Some(String::new()),
        "~." => return Some(".".to_owned()),
        "~.." => return Some("..".to_owned()),
        _ => {}
    }
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars();
    while let Some(c) = chars.next() {
        if c == '~' {
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Encodes a key as a token, the form listings show. Returns `None` for keys
/// containing NUL, which no path can carry.
#[must_use]
pub fn encode_token(key: &str) -> Option<String> {
    if key.contains('\0') {
        return None;
    }
    Some(match key {
        "" => "~e".to_owned(),
        "." => "~.".to_owned(),
        ".." => "~..".to_owned(),
        _ => key.replace('~', "~0").replace('/', "~1"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6901() {
        assert_eq!(decode_token("a~1b").as_deref(), Some("a/b"));
        assert_eq!(decode_token("m~0n").as_deref(), Some("m~n"));
        // `~01` is `~` then `1`, not `/`.
        assert_eq!(decode_token("~01").as_deref(), Some("~1"));
        assert_eq!(decode_token("plain").as_deref(), Some("plain"));
        assert_eq!(decode_token("a@b#").as_deref(), Some("a@b#"));
        for bad in ["~", "a~", "~2", "~x", "a~.b", "~e~e", "~...", "~.e"] {
            assert_eq!(decode_token(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn extension() {
        assert_eq!(decode_token("~e").as_deref(), Some(""));
        assert_eq!(decode_token("~.").as_deref(), Some("."));
        assert_eq!(decode_token("~..").as_deref(), Some(".."));
        assert_eq!(encode_token("").as_deref(), Some("~e"));
        assert_eq!(encode_token(".").as_deref(), Some("~."));
        assert_eq!(encode_token("..").as_deref(), Some("~.."));
        assert_eq!(encode_token("..."), Some("...".to_owned()));
        assert_eq!(encode_token("a\0b"), None);
    }

    #[test]
    fn roundtrip() {
        for key in [
            "", ".", "..", "~", "/", "~1", "a/b~c", "~e", "~.", "ünï", "a b", "#", "@",
        ] {
            let token = encode_token(key).unwrap();
            assert!(!token.contains('/'), "{token:?}");
            assert_eq!(decode_token(&token).as_deref(), Some(key), "{key:?}");
        }
    }
}
