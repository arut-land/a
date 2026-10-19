//! Datastar's `datastar-patch-elements` server-sent events: how a live tab
//! is told what changed.
//!
//! Framed here rather than through the `datastar` crate: its builder holds
//! each event as a `Vec<String>` of lines before formatting it, and splits
//! only at `\n`, while a lone `\r` (a progress bar's, in a command's output)
//! also ends an event-stream line.

/// One change to a tab's page.
pub enum Patch<'a> {
    /// Morphs the element with the id of `html`'s top element into it.
    Replace(&'a str),
    /// Adds `html` at the end of the children of the element `selector`.
    Append { selector: &'a str, html: &'a str },
    /// Adds `html` before the element `selector`.
    Before { selector: &'a str, html: &'a str },
}

impl Patch<'_> {
    /// Appends the event to `out`.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        let (selector, mode, html) = match *self {
            Self::Replace(html) => (None, None, html),
            Self::Append { selector, html } => (Some(selector), Some("append"), html),
            Self::Before { selector, html } => (Some(selector), Some("before"), html),
        };
        out.extend_from_slice(b"event: datastar-patch-elements\n");
        if let Some(selector) = selector {
            line(out, "selector ", selector);
        }
        if let Some(mode) = mode {
            line(out, "mode ", mode);
        }
        // Datastar joins an event's `elements` lines with `\n` again.
        for part in html.split('\n') {
            line(out, "elements ", part);
        }
        out.push(b'\n');
    }
}

fn line(out: &mut Vec<u8>, key: &str, value: &str) {
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(key.as_bytes());
    // In markup, a character reference stands for the `\r` as well.
    let mut parts = value.split('\r');
    out.extend_from_slice(parts.next().unwrap_or_default().as_bytes());
    for part in parts {
        out.extend_from_slice(b"&#13;");
        out.extend_from_slice(part.as_bytes());
    }
    out.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::Patch;

    fn framed(patch: &Patch) -> String {
        let mut out = Vec::new();
        patch.write_to(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn an_event_is_one_data_line_per_markup_line() {
        assert_eq!(
            framed(&Patch::Replace("<p id=\"a\">x\ny</p>")),
            "event: datastar-patch-elements\n\
             data: elements <p id=\"a\">x\n\
             data: elements y</p>\n\n"
        );
        assert_eq!(
            framed(&Patch::Before {
                selector: "#end",
                html: "<p>z</p>"
            }),
            "event: datastar-patch-elements\n\
             data: selector #end\n\
             data: mode before\n\
             data: elements <p>z</p>\n\n"
        );
    }

    #[test]
    fn a_carriage_return_does_not_end_a_line() {
        let event = framed(&Patch::Append {
            selector: "#r",
            html: "<span>10%\r20%\r\n</span>",
        });
        assert!(!event.contains('\r'), "{event:?}");
        assert!(
            event.contains("data: elements <span>10%&#13;20%&#13;\ndata: elements </span>\n"),
            "{event:?}"
        );
    }
}
