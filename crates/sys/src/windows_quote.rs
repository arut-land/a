/// MS C argv parsing doubles backslashes only before quotes and the closing
/// quote. Always quoting also covers empty arguments and whitespace.
pub(super) fn quote(arg: &[u16]) -> Vec<u16> {
    let (slash, quote) = (u16::from(b'\\'), u16::from(b'"'));
    let mut out = vec![quote];
    let mut slashes = 0;
    for &unit in arg {
        if unit == slash {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            slash,
            slashes * if unit == quote { 2 } else { 1 },
        ));
        if unit == quote {
            out.push(slash);
        }
        out.push(unit);
        slashes = 0;
    }
    out.extend(std::iter::repeat_n(slash, slashes * 2));
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_spaces_quotes_and_trailing_slashes_keep_their_argument() {
        for (input, expected) in [
            ("", "\"\""),
            ("a b", "\"a b\""),
            ("a\"b", "\"a\\\"b\""),
            ("c:\\a\\", "\"c:\\a\\\\\""),
        ] {
            assert_eq!(
                String::from_utf16(&quote(&input.encode_utf16().collect::<Vec<_>>())).unwrap(),
                expected
            );
        }
    }
}
