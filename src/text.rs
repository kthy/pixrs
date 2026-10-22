//! Text from outside pixrs, such as file names and the messages of external tools, made safe to
//! print: a file name can hold control characters, which a terminal acts on.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use std::borrow::Cow;
use std::fmt;
use std::path::Path;

/// `text` with its control characters escaped as Rust writes them (`\u{1b}`, `\n`), so that
/// printing it cannot move the cursor, change colors or the window title, or start a line of
/// its own. Text without control characters comes back as it is.
pub fn printable(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut escaped = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if c.is_control() {
            escaped.extend(c.escape_default());
        } else {
            escaped.push(c);
        }
    }
    Cow::Owned(escaped)
}

/// `printable` for a message of several lines: its line breaks stay, every other control
/// character is escaped.
pub fn printable_lines(text: &str) -> String {
    text.split('\n')
        .map(printable)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A path to print: as `Path::display` shows it, with control characters escaped.
pub struct Shown<'a>(&'a Path);

/// `path`, to print.
pub fn shown(path: &Path) -> Shown<'_> {
    Shown(path)
}

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*printable(&self.0.to_string_lossy()), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn test_printable() {
        assert!(matches!(
            printable("Sommer på Ærø 2019 ワイフ.jpg"),
            Cow::Borrowed(_)
        ));
        // A window title, a cleared screen, a line that is not one, and C1 controls.
        assert_eq!(
            printable("a\u{1b}]0;owned\u{7}b\u{1b}[2J.jpg"),
            "a\\u{1b}]0;owned\\u{7}b\\u{1b}[2J.jpg"
        );
        assert_eq!(
            printable("x\nDone.\rtab\tdel\u{7f}csi\u{9b}"),
            "x\\nDone.\\rtab\\tdel\\u{7f}csi\\u{9b}"
        );
        assert_eq!(
            printable_lines("one \u{1b}[31mred\ntwo\r"),
            "one \\u{1b}[31mred\ntwo\\r"
        );
    }

    #[test]
    fn test_shown() {
        let path = Path::new("lib").join("x\u{1b}[31my.jpg");
        assert_eq!(
            shown(&path).to_string(),
            format!("lib{}x\\u{{1b}}[31my.jpg", std::path::MAIN_SEPARATOR)
        );
        assert_eq!(shown(Path::new("plain.jpg")).to_string(), "plain.jpg");
    }
}
