//! Translating between source folder names and the names we present over IMAP.
//!
//! We present `/` as our hierarchy delimiter. Sources differ: Dovecot with
//! Maildir++ uses `.`, Gmail and Exchange use `/`. A folder is stored under the
//! name its source gave it, and translated only for display.
//!
//! **Why this is lossless rather than lossy:** a server cannot have a folder
//! whose name contains its own delimiter as data — it would be read as
//! hierarchy. So substituting the *source's own* delimiter is reversible.
//! Assuming a delimiter the server does not use is what mangles names, which is
//! why the delimiter is recorded per account rather than guessed.

pub const OURS: char = '/';

/// Source name -> the name shown to clients, within an account namespace.
pub fn to_display(source: &str, delimiter: Option<char>) -> String {
    match delimiter {
        Some(d) if d != OURS => source.replace(d, &OURS.to_string()),
        // Unknown delimiter, or the source already uses ours: pass through
        // unchanged rather than guessing.
        _ => source.to_string(),
    }
}

/// The name a client sent -> the source name to look up.
pub fn from_display(display: &str, delimiter: Option<char>) -> String {
    match delimiter {
        Some(d) if d != OURS => display.replace(OURS, &d.to_string()),
        _ => display.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dovecot_dots_become_a_tree() {
        assert_eq!(
            to_display("Archives.qra.2014.Sent", Some('.')),
            "Archives/qra/2014/Sent"
        );
    }

    #[test]
    fn gmail_names_are_untouched() {
        // Gmail's delimiter is already '/', so nothing should change — and a
        // label containing a dot must survive intact.
        assert_eq!(
            to_display("[Gmail]/All Mail", Some('/')),
            "[Gmail]/All Mail"
        );
        assert_eq!(to_display("receipts.2024", Some('/')), "receipts.2024");
    }

    #[test]
    fn unknown_delimiter_passes_through() {
        // Better to show an odd flat name than to invent hierarchy that the
        // source never had.
        assert_eq!(to_display("Archives.2014", None), "Archives.2014");
    }

    #[test]
    fn round_trips() {
        for (source, delim) in [
            ("INBOX", Some('.')),
            ("Archives.qra.2014.Sent", Some('.')),
            ("[Gmail]/All Mail", Some('/')),
            ("receipts.2024", Some('/')),
            ("Odd.Name", None),
        ] {
            let shown = to_display(source, delim);
            assert_eq!(
                from_display(&shown, delim),
                source,
                "{source:?} with delimiter {delim:?} did not round-trip"
            );
        }
    }
}
