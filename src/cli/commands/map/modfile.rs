//! `go.mod` and `go.work` as `golang.org/x/mod/modfile` reads them.
//!
//! The grammar is small enough to read whole: lines of tokens — a bare
//! word, a `"…"` string with Go's escapes, a `` `…` `` raw string — with
//! `//` comments, and a verb factored over a parenthesised block of lines
//! (`use ( ./a ./b )`, one entry a line). What this reader does not take,
//! it refuses with the reason rather than reading past: `/* */` comments,
//! which modfile refuses too, a string a newline or the end cuts short, an
//! escape Go does not define, a bracket or comma, a parenthesis out of
//! place. A file it refuses is one the caller says it could not read, so
//! nothing it holds goes missing without a word.

/// One directive: its verb and its arguments, strings unquoted. A block's
/// lines are directives of the block's verb.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Directive {
    pub(super) verb: String,
    pub(super) args: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    /// A bare word or a string, unquoted; `quoted` keeps a string from
    /// being read as a parenthesis or a verb.
    Text {
        text: String,
        quoted: bool,
    },
    Open,
    Close,
    Newline,
}

/// The directives `text` holds, or why it is no file modfile reads.
pub(super) fn parse(text: &str) -> Result<Vec<Directive>, String> {
    let tokens = lex(text)?;
    let mut lines: Vec<(usize, Vec<Token>)> = Vec::new();
    let mut line = 1;
    let mut current = Vec::new();
    for token in tokens {
        if token == Token::Newline {
            if !current.is_empty() {
                lines.push((line, std::mem::take(&mut current)));
            }
            line += 1;
        } else {
            current.push(token);
        }
    }
    if !current.is_empty() {
        lines.push((line, current));
    }

    let mut directives = Vec::new();
    let mut lines = lines.into_iter();
    while let Some((at, tokens)) = lines.next() {
        let mut words = Vec::with_capacity(tokens.len());
        let mut opens_block = false;
        for (index, token) in tokens.iter().enumerate() {
            match token {
                Token::Text { text, .. } => words.push(text.clone()),
                Token::Open if index == 1 && index + 1 == tokens.len() => opens_block = true,
                Token::Open => return Err(format!("line {at}: unexpected (")),
                Token::Close => return Err(format!("line {at}: unexpected )")),
                Token::Newline => {}
            }
        }
        if matches!(tokens.first(), Some(Token::Text { quoted: true, .. })) {
            return Err(format!("line {at}: a directive starts with a string"));
        }
        let mut words = words.into_iter();
        let Some(verb) = words.next() else {
            continue;
        };
        if !opens_block {
            directives.push(Directive {
                verb,
                args: words.collect(),
            });
            continue;
        }
        loop {
            let Some((inner, tokens)) = lines.next() else {
                return Err(format!("line {at}: the {verb} block is not closed"));
            };
            if tokens == [Token::Close] {
                break;
            }
            let mut args = Vec::with_capacity(tokens.len());
            for token in tokens {
                match token {
                    Token::Text { text, .. } => args.push(text),
                    Token::Open => return Err(format!("line {inner}: unexpected (")),
                    Token::Close => {
                        return Err(format!(
                            "line {inner}: ) closes a block on a line of its own"
                        ));
                    }
                    Token::Newline => {}
                }
            }
            directives.push(Directive {
                verb: verb.clone(),
                args,
            });
        }
    }
    Ok(directives)
}

/// The module path a `go.mod` declares, `None` where it declares none.
pub(super) fn module_path(go_mod: &str) -> Result<Option<String>, String> {
    let directives = parse(go_mod)?;
    let mut modules = directives.iter().filter(|d| d.verb == "module");
    let Some(module) = modules.next() else {
        return Ok(None);
    };
    if modules.next().is_some() {
        return Err("it repeats the module directive".to_string());
    }
    match module.args.as_slice() {
        [path] => Ok(Some(path.clone())),
        _ => Err("its module directive does not name one path".to_string()),
    }
}

/// Whether modfile reads `c` as part of a bare word: printable, no space,
/// none of `()[]{},`.
fn is_word(c: char) -> bool {
    !matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',')
        && !c.is_whitespace()
        && !c.is_control()
        && c != '\u{feff}'
}

fn lex(text: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    let mut line = 1;
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\r' => {
                chars.next();
            }
            '\n' => {
                chars.next();
                tokens.push(Token::Newline);
                line += 1;
            }
            '(' => {
                chars.next();
                tokens.push(Token::Open);
            }
            ')' => {
                chars.next();
                tokens.push(Token::Close);
            }
            '"' | '`' => {
                chars.next();
                let mut raw = String::new();
                loop {
                    match chars.next() {
                        None => return Err(format!("line {line}: a string runs to the end")),
                        Some('\n') => {
                            return Err(format!("line {line}: a newline cuts a string short"));
                        }
                        Some(end) if end == c => break,
                        Some('\\') if c == '"' => {
                            raw.push('\\');
                            match chars.next() {
                                None | Some('\n') => {
                                    return Err(format!("line {line}: a string is cut short"));
                                }
                                Some(escaped) => raw.push(escaped),
                            }
                        }
                        Some(other) => raw.push(other),
                    }
                }
                let text = if c == '"' {
                    unquote(&raw).map_err(|e| format!("line {line}: {e}"))?
                } else {
                    raw
                };
                tokens.push(Token::Text { text, quoted: true });
            }
            '/' if text_at(&chars, "//") => {
                while chars.peek().is_some_and(|&c| c != '\n') {
                    chars.next();
                }
            }
            '/' if text_at(&chars, "/*") => {
                return Err(format!("line {line}: modfile reads // comments, not /* */"));
            }
            c if is_word(c) => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if !is_word(c) || text_at(&chars, "//") {
                        break;
                    }
                    if text_at(&chars, "/*") {
                        return Err(format!("line {line}: modfile reads // comments, not /* */"));
                    }
                    word.push(c);
                    chars.next();
                }
                tokens.push(Token::Text {
                    text: word,
                    quoted: false,
                });
            }
            c => return Err(format!("line {line}: unexpected character {c:?}")),
        }
    }
    Ok(tokens)
}

fn text_at(chars: &std::iter::Peekable<std::str::Chars>, prefix: &str) -> bool {
    chars.clone().take(prefix.len()).eq(prefix.chars())
}

/// The value a Go interpreted string's body spells, as `strconv.Unquote`
/// reads it.
fn unquote(body: &str) -> Result<String, String> {
    let mut value = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            value.push(c);
            continue;
        }
        let escaped = chars.next().ok_or("a string ends in \\")?;
        let simple = match escaped {
            'a' => Some('\u{7}'),
            'b' => Some('\u{8}'),
            'f' => Some('\u{c}'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'v' => Some('\u{b}'),
            '\\' => Some('\\'),
            '"' => Some('"'),
            _ => None,
        };
        if let Some(simple) = simple {
            value.push(simple);
            continue;
        }
        let (digits, radix) = match escaped {
            'x' => (2, 16),
            'u' => (4, 16),
            'U' => (8, 16),
            '0'..='7' => (2, 8),
            other => return Err(format!("\\{other} is no Go escape")),
        };
        let mut code = if radix == 8 {
            escaped.to_digit(8).unwrap_or_default()
        } else {
            0
        };
        for _ in 0..digits {
            let digit = chars
                .next()
                .and_then(|d| d.to_digit(radix))
                .ok_or_else(|| format!("\\{escaped} is cut short"))?;
            code = code * radix + digit;
        }
        match escaped {
            // A byte escape: modfile's paths are UTF-8, so one that does not
            // spell a character on its own is no path it takes either.
            'x' | '0'..='7' if code < 0x80 => value.push(char::from(code as u8)),
            'x' | '0'..='7' => return Err(format!("\\{escaped} spells a byte, not a character")),
            _ => value.push(char::from_u32(code).ok_or("an escape names no character")?),
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every token form, a comment and a factored block read as modfile
    /// reads them: a block's lines are directives of its verb.
    #[test]
    fn a_modfile_reads_every_token_form() {
        let text = "go 1.22 // the toolchain\n\nuse ./a\nuse (\n\t./b // a comment\n\t\"./c d\"\n\t`./e`\n\n)\nuse \"./f\\u00e9\\t\\101\"\n";
        let directive = |verb: &str, args: &[&str]| Directive {
            verb: verb.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
        };
        assert_eq!(
            parse(text).unwrap(),
            [
                directive("go", &["1.22"]),
                directive("use", &["./a"]),
                directive("use", &["./b"]),
                directive("use", &["./c d"]),
                directive("use", &["./e"]),
                directive("use", &["./fé\tA"]),
            ]
        );
    }

    #[test]
    fn a_go_mod_declares_its_module_in_any_form() {
        assert_eq!(
            module_path("// c\nmodule example.com/m // x\n\ngo 1.22\n").unwrap(),
            Some("example.com/m".to_string())
        );
        assert_eq!(
            module_path("module \"example.com/q\"\n").unwrap(),
            Some("example.com/q".to_string())
        );
        assert_eq!(
            module_path("module (\n\t`example.com/b`\n)\n").unwrap(),
            Some("example.com/b".to_string())
        );
        assert_eq!(module_path("go 1.22\n").unwrap(), None);
    }

    /// What the reader does not take is refused with its reason, never
    /// read past: a member it skipped would be one the answer lost.
    #[test]
    fn what_modfile_would_refuse_is_refused() {
        for (text, reason) in [
            ("use (\n\t./a\n", "not closed"),
            ("use \"./a\n", "newline"),
            ("use `./a", "end"),
            ("use \"./\\q\"\n", "no Go escape"),
            ("use \"./\\xff\"\n", "byte"),
            ("/* c */\nuse ./a\n", "/* */"),
            ("use ./a, ./b\n", "unexpected character"),
            ("use (./a)\n", "unexpected"),
            ("use (\n\t./a )\n)\n", "line of its own"),
            ("\"use\" ./a\n", "starts with a string"),
        ] {
            let error = parse(text).unwrap_err();
            assert!(error.contains(reason), "{text:?}: {error}");
        }
        assert!(
            module_path("module a\nmodule b\n")
                .unwrap_err()
                .contains("repeats")
        );
    }
}
