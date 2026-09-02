//! Extracting the parts of a message that IMAP `FETCH` can ask for.
//!
//! Operates on the **original bytes** from S3 and never re-encodes. A client
//! asking for `BODY[]` gets exactly what arrived, so signatures still verify and
//! encoding quirks survive — including in messages our MIME parser would
//! mishandle.

/// Split a raw message into (header, body), both borrowed from the original.
///
/// The header includes its terminating blank line, as IMAP expects. A message
/// with no blank line at all is treated as all-header, which is what a truncated
/// or malformed message effectively is.
pub fn split_header_body(raw: &[u8]) -> (&[u8], &[u8]) {
    // Look for CRLFCRLF first, then bare LFLF: real archives contain both.
    if let Some(i) = find(raw, b"\r\n\r\n") {
        return (&raw[..i + 4], &raw[i + 4..]);
    }
    if let Some(i) = find(raw, b"\n\n") {
        return (&raw[..i + 2], &raw[i + 2..]);
    }
    (raw, &[])
}

/// Header lines whose field name matches one of `wanted` (case-insensitive),
/// including folded continuation lines, terminated by a blank line.
///
/// Thunderbird uses this constantly to build a message list without downloading
/// bodies, so it is worth getting the folding right: a continuation line belongs
/// to the field above it, and dropping it would silently truncate long subjects.
pub fn header_fields(raw: &[u8], wanted: &[String]) -> Vec<u8> {
    let (header, _) = split_header_body(raw);
    let want: Vec<String> = wanted.iter().map(|w| w.to_ascii_lowercase()).collect();

    let mut out = Vec::new();
    let mut keeping = false;

    for line in split_lines(header) {
        let is_continuation = line.starts_with(b" ") || line.starts_with(b"\t");
        if is_continuation {
            if keeping {
                out.extend_from_slice(line);
            }
            continue;
        }

        keeping = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let name = String::from_utf8_lossy(&line[..colon]).to_ascii_lowercase();
                want.iter().any(|w| *w == name)
            }
            None => false,
        };
        if keeping {
            out.extend_from_slice(line);
        }
    }

    out.extend_from_slice(b"\r\n");
    out
}

/// Apply a `<offset.length>` partial fetch.
///
/// Reading past the end truncates rather than erroring — required by the spec,
/// and clients do it routinely when resuming a partial download.
pub fn partial(data: &[u8], offset: u32, length: u32) -> &[u8] {
    let start = (offset as usize).min(data.len());
    let end = start.saturating_add(length as usize).min(data.len());
    &data[start..end]
}

/// Lines including their terminators, so reassembly is byte-exact.
fn split_lines(mut data: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    while !data.is_empty() {
        match data.iter().position(|&b| b == b'\n') {
            Some(i) => {
                lines.push(&data[..=i]);
                data = &data[i + 1..];
            }
            None => {
                lines.push(data);
                break;
            }
        }
    }
    lines
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSG: &[u8] = b"Subject: Hello\r\nFrom: a@b.c\r\nX-Long: one\r\n  two\r\n\r\nbody here\r\n";

    #[test]
    fn splits_on_crlf() {
        let (h, b) = split_header_body(MSG);
        assert!(h.ends_with(b"\r\n\r\n"));
        assert_eq!(b, b"body here\r\n");
    }

    #[test]
    fn splits_on_bare_lf() {
        // Plenty of twenty-year-old mail is stored with bare LF.
        let (h, b) = split_header_body(b"Subject: x\n\nbody");
        assert_eq!(h, b"Subject: x\n\n");
        assert_eq!(b, b"body");
    }

    #[test]
    fn message_without_a_blank_line_is_all_header() {
        let (h, b) = split_header_body(b"Subject: truncated");
        assert_eq!(h, b"Subject: truncated");
        assert!(b.is_empty());
    }

    #[test]
    fn selects_fields_case_insensitively() {
        let out = header_fields(MSG, &["SUBJECT".into()]);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("Subject: Hello"), "{s}");
        assert!(!s.contains("From:"), "{s}");
    }

    #[test]
    fn keeps_folded_continuation_lines() {
        // Dropping the continuation would silently truncate a long subject.
        let out = header_fields(MSG, &["x-long".into()]);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("one"), "{s}");
        assert!(s.contains("two"), "missing folded line: {s}");
    }

    #[test]
    fn continuation_of_an_unwanted_field_is_dropped() {
        let out = header_fields(MSG, &["subject".into()]);
        assert!(!String::from_utf8_lossy(&out).contains("two"));
    }

    #[test]
    fn partial_truncates_past_the_end() {
        assert_eq!(partial(b"abcdef", 2, 100), b"cdef");
        assert_eq!(partial(b"abcdef", 99, 10), b"");
    }
}
