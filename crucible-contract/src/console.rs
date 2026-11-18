//! Console text as programs write it to a terminal: color and cursor escapes are dropped before
//! a line is matched or shown.

use std::borrow::Cow;

/// `text` without its ANSI escape sequences: CSI (`ESC [ … final`), OSC (`ESC ] … BEL` or
/// `ESC ] … ESC \`), and the rest (`ESC`, any intermediate bytes, one final).
pub fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.contains('\u{1b}') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
                for c in chars.by_ref() {
                    if !('\u{20}'..='\u{2f}').contains(&c) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use crate::console::strip_ansi;

    /// A `tracing_subscriber` line with spans, as an `[mcp]` server wrote it with colors on.
    pub const COLORED: &str = "\u{1b}[2m2026-11-09T18:02:41.069974Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[1mcve-triage\u{1b}[0m\u{1b}[1m{\u{1b}[0m\u{1b}[3mcommand\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"discover\"\u{1b}[1m}\u{1b}[0m\u{1b}[2m:\u{1b}[0m\u{1b}[1mtriage.prepare\u{1b}[0m\u{1b}[1m{\u{1b}[0m\u{1b}[3mprofile\u{1b}[0m\u{1b}[2m=\u{1b}[0mllm-d\u{1b}[1m}\u{1b}[0m\u{1b}[2m:\u{1b}[0m \u{1b}[2mcve_triage::tickets\u{1b}[0m\u{1b}[2m:\u{1b}[0m [llm-d] Jira search returned 176 ticket(s)";

    #[test]
    fn a_colored_tracing_line_reads_as_its_text() {
        assert_eq!(
            strip_ansi(COLORED),
            "2026-11-09T18:02:41.069974Z  INFO cve-triage{command=\"discover\"}:triage.prepare{profile=llm-d}: cve_triage::tickets: [llm-d] Jira search returned 176 ticket(s)"
        );
    }

    #[test]
    fn plain_text_is_borrowed_and_other_escapes_go_whole() {
        assert!(matches!(
            strip_ansi("no escapes [2m here"),
            std::borrow::Cow::Borrowed("no escapes [2m here")
        ));
        assert_eq!(strip_ansi("a\u{1b}[38;5;208mb\u{1b}[Kc"), "abc");
        assert_eq!(
            strip_ansi("\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{1b}\\ done"),
            "link done"
        );
        assert_eq!(strip_ansi("x\u{1b}(By"), "xy");
        assert_eq!(strip_ansi("cut off \u{1b}[3"), "cut off ");
    }
}
