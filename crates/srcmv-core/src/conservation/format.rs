//! Line-edge normalization backing formatting-only classification.
//!
//! The transformation `W` is pinned by plan §8.7: EOL variants collapse to LF
//! in a single left-to-right pass, lines split terminator-exclusively, ASCII
//! whitespace strips from both ends of every line, and empty lines drop
//! before an exact sequence comparison. Byte purity is preserved elsewhere:
//! a qualifying rewrite is reported as formatting-only and never becomes a
//! conservation match.

/// Applies line-edge normalization `W` to `bytes`.
///
/// One left-to-right pass replaces every CRLF pair and lone CR byte with LF;
/// the normalized bytes then split into terminator-exclusive line contents,
/// where a trailing terminator starts no additional line, empty input has
/// zero lines, and a lone terminator is one empty line. Leading and trailing
/// ASCII whitespace strips from every line and empty lines drop from the
/// result.
#[must_use]
pub(crate) fn normalize_line_edges(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut line_start = 0_usize;
    let mut offset = 0_usize;
    while offset < bytes.len() {
        match bytes[offset] {
            b'\r' => {
                push_stripped_line(&mut lines, &bytes[line_start..offset]);
                offset += 1 + usize::from(bytes.get(offset + 1) == Some(&b'\n'));
                line_start = offset;
            }
            b'\n' => {
                push_stripped_line(&mut lines, &bytes[line_start..offset]);
                offset += 1;
                line_start = offset;
            }
            _ => offset += 1,
        }
    }
    if line_start < bytes.len() {
        push_stripped_line(&mut lines, &bytes[line_start..]);
    }
    lines.retain(|line| !line.is_empty());
    lines
}

fn push_stripped_line(lines: &mut Vec<Vec<u8>>, raw: &[u8]) {
    let stripped = raw.trim_ascii();
    if !stripped.is_empty() {
        lines.push(stripped.to_vec());
    }
}

/// Returns whether a removed region equals its paired insertion context
/// under line-edge normalization.
///
/// Deleted-file regions never reach this check because they carry no paired
/// context; the caller gates applicability per §8.7.
#[must_use]
pub(crate) fn formatting_equivalent(region: &[u8], paired_remainder: &[u8]) -> bool {
    normalize_line_edges(region) == normalize_line_edges(paired_remainder)
}

#[cfg(test)]
mod tests {
    use super::{formatting_equivalent, normalize_line_edges};

    fn lines(bytes: &[u8]) -> Vec<Vec<u8>> {
        normalize_line_edges(bytes)
    }

    fn text<const N: usize>(rows: [&[u8]; N]) -> Vec<Vec<u8>> {
        rows.iter().map(|row| row.to_vec()).collect()
    }

    #[test]
    fn normalization_should_match_every_worked_example_from_the_plan() {
        assert_eq!(lines(b"\r"), Vec::<Vec<u8>>::new());
        assert_eq!(lines(b""), Vec::<Vec<u8>>::new());
        assert_eq!(lines(b"a\n"), text([b"a"]));
        assert_eq!(lines(b"  a\n\tb\r\n"), text([b"a", b"b"]));
        assert_eq!(lines(b"a\n\nb"), lines(b"a\nb"));
        assert_eq!(lines(b"a\n\nb"), text([b"a", b"b"]));
    }

    #[test]
    fn normalization_should_treat_lone_terminators_as_one_empty_line() {
        assert_eq!(lines(b"\n"), Vec::<Vec<u8>>::new());
        assert_eq!(lines(b"\r"), Vec::<Vec<u8>>::new());
        assert_eq!(lines(b"\r\n"), Vec::<Vec<u8>>::new());
        assert_eq!(lines(b"\n\n"), Vec::<Vec<u8>>::new());
        assert_eq!(lines(b"a\r"), lines(b"a"));
    }

    #[test]
    fn normalization_should_collapse_eol_and_indent_variants_to_one_class() {
        let expected = text([b"first", b"second"]);
        assert_eq!(lines(b"first\r\nsecond\r\n"), expected);
        assert_eq!(lines(b"first\rsecond\r"), expected);
        assert_eq!(lines(b"first\nsecond"), expected);
        assert_eq!(lines(b"  first\n\tsecond  \n"), expected);
        assert_eq!(lines(b"first\n\n\nsecond\n"), expected);
    }

    #[test]
    fn formatting_equivalence_should_hold_for_edge_only_rewrites() {
        assert!(formatting_equivalent(
            b"alpha\r\nbravo\r\n",
            b"alpha\nbravo\n"
        ));
        assert!(formatting_equivalent(b"\r", b""));
        assert!(formatting_equivalent(b"    x\n", b"\tx\n"));
        assert!(formatting_equivalent(b"x\n\n\ny\n", b"x\ny\n"));
    }

    #[test]
    fn formatting_equivalence_should_reject_intra_line_spacing_changes() {
        assert!(!formatting_equivalent(b"a  b\n", b"a b\n"));
        assert!(!formatting_equivalent(b"a b\nc d\n", b"ab\ncd\n"));
    }

    #[test]
    fn formatting_equivalence_should_reject_deleted_line_breaks() {
        // Joining two statements yields one nonempty line where there were two.
        assert!(!formatting_equivalent(
            b"let a = 1;\nlet b = 2;\n",
            b"let a = 1; let b = 2;\n"
        ));
        assert!(!formatting_equivalent(b"one\ntwo\n", b"onetwo\n"));
    }

    #[test]
    fn normalization_should_keep_interior_whitespace_of_lines() {
        assert_eq!(lines(b"  a b  \n"), text([b"a b"]));
        assert_eq!(lines(b"a\tb\n"), text([b"a\tb"]));
    }
}
