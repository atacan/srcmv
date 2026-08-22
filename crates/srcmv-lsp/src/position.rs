//! Exact conversion between LSP coordinates and immutable snapshot bytes.

use std::fmt;

use gen_lsp_types::{Position, Range};
use srcmv_core::{ByteRange, LineIndex};

use crate::capabilities::SupportedPositionEncoding;

/// Default maximum number of Unicode scalar values examined by one converter.
pub const DEFAULT_MAXIMUM_CODE_POINTS_SCANNED: u64 = 16 * 1024 * 1024;

/// Cumulative work limits for position conversion over one snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PositionLimits {
    /// Maximum Unicode scalar values that conversions may examine.
    ///
    /// UTF-8 conversions that can validate a byte boundary in constant time do
    /// not consume this budget. UTF-16, UTF-32, and user scalar-column scans
    /// consume one unit for each examined scalar value.
    pub maximum_code_points_scanned: u64,
}

impl Default for PositionLimits {
    fn default() -> Self {
        Self {
            maximum_code_points_scanned: DEFAULT_MAXIMUM_CODE_POINTS_SCANNED,
        }
    }
}

/// A non-sensitive reason a snapshot position could not be converted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionError {
    /// The line index does not describe a snapshot with this byte length.
    InvalidSnapshotIndex,
    /// An LSP position refers to a line that does not exist.
    NonexistentLine,
    /// An LSP character offset splits a UTF-8 code point or UTF-16 surrogate pair.
    CharacterSplitsCodeUnit,
    /// An LSP range is reversed, empty, or otherwise outside the snapshot.
    InvalidRange,
    /// A user byte, line, or scalar-column coordinate is outside the snapshot.
    InvalidUserPosition,
    /// A byte offset splits a UTF-8 code point or lies inside a line terminator.
    ByteNotRepresentable,
    /// Cumulative Unicode scanning exceeded the configured work limit.
    WorkLimitExceeded,
}

impl fmt::Display for PositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSnapshotIndex => "line index does not describe the immutable snapshot",
            Self::NonexistentLine => "position refers to a nonexistent line",
            Self::CharacterSplitsCodeUnit => {
                "position splits a UTF-8 code point or UTF-16 surrogate pair"
            }
            Self::InvalidRange => "range is reversed, empty, or outside the snapshot",
            Self::InvalidUserPosition => "user position is outside the immutable snapshot",
            Self::ByteNotRepresentable => "byte offset is not representable as an LSP position",
            Self::WorkLimitExceeded => "position conversion work limit exceeded",
        })
    }
}

impl std::error::Error for PositionError {}

/// Converts positions against one immutable UTF-8 snapshot and its line index.
///
/// The supplied [`LineIndex`] must have been built from `text.as_bytes()`.
/// Conversion never reads from the filesystem and never changes the snapshot.
/// Work accounting is cumulative so a caller can use one converter for all
/// symbols returned by a server.
pub struct PositionConverter<'a> {
    text: &'a str,
    line_index: &'a LineIndex,
    encoding: SupportedPositionEncoding,
    limits: PositionLimits,
    code_points_scanned: u64,
}

impl<'a> PositionConverter<'a> {
    /// Creates a converter after checking the index's snapshot byte length.
    ///
    /// # Errors
    ///
    /// Returns [`PositionError::InvalidSnapshotIndex`] when `line_index` was
    /// built for a snapshot with a different byte length.
    pub fn new(
        text: &'a str,
        line_index: &'a LineIndex,
        encoding: SupportedPositionEncoding,
        limits: PositionLimits,
    ) -> Result<Self, PositionError> {
        let byte_length =
            u64::try_from(text.len()).map_err(|_| PositionError::InvalidSnapshotIndex)?;
        line_index
            .metrics_for_range(
                text.as_bytes(),
                ByteRange {
                    start: 0,
                    end: byte_length,
                },
            )
            .map_err(|_| PositionError::InvalidSnapshotIndex)?;

        Ok(Self {
            text,
            line_index,
            encoding,
            limits,
            code_points_scanned: 0,
        })
    }

    /// Returns the negotiated encoding used by this converter.
    #[must_use]
    pub const fn encoding(&self) -> SupportedPositionEncoding {
        self.encoding
    }

    /// Returns the cumulative Unicode scalar values examined so far.
    #[must_use]
    pub const fn code_points_scanned(&self) -> u64 {
        self.code_points_scanned
    }

    /// Converts a server position to an exact snapshot byte offset.
    ///
    /// An oversized character on an existing line is normalized to that
    /// line's content end. CR and LF terminator bytes never contribute to the
    /// character count.
    ///
    /// # Errors
    ///
    /// Returns a typed error for a nonexistent line, a split code-unit
    /// boundary, an invalid snapshot index, or exhausted conversion work.
    pub fn lsp_position_to_byte(&mut self, position: Position) -> Result<u64, PositionError> {
        let bounds = self.line_bounds(position.line)?;
        let content = self.line_content(bounds)?;
        let relative = match self.encoding {
            SupportedPositionEncoding::Utf8 => utf8_units_to_byte(content, position.character)?,
            SupportedPositionEncoding::Utf16 => {
                self.encoded_units_to_byte(content, position.character, char::len_utf16)?
            }
            SupportedPositionEncoding::Utf32 => {
                self.encoded_units_to_byte(content, position.character, |_| 1)?
            }
        };
        bounds
            .start
            .checked_add(relative)
            .ok_or(PositionError::InvalidSnapshotIndex)
    }

    /// Converts and validates a nonempty server range.
    ///
    /// Both endpoints use LSP oversized-character normalization before the
    /// resulting byte range is checked for ordering and non-emptiness.
    ///
    /// # Errors
    ///
    /// Returns [`PositionError::InvalidRange`] when the normalized range is
    /// empty or reversed. Endpoint conversion errors are returned unchanged.
    pub fn lsp_range_to_byte_range(&mut self, range: Range) -> Result<ByteRange, PositionError> {
        let start = self.lsp_position_to_byte(range.start)?;
        let end = self.lsp_position_to_byte(range.end)?;
        if start >= end || end > self.byte_length()? {
            return Err(PositionError::InvalidRange);
        }
        Ok(ByteRange { start, end })
    }

    /// Converts a representable snapshot byte offset to an LSP position.
    ///
    /// Content starts and ends are representable. Offsets splitting a UTF-8
    /// code point, offsets inside CR/LF terminators, and EOF after a final
    /// terminator are not representable because LSP has no corresponding
    /// physical-line character position.
    ///
    /// # Errors
    ///
    /// Returns [`PositionError::ByteNotRepresentable`] when `byte_offset` has
    /// no exact LSP representation, or a work-limit error while counting units.
    pub fn byte_to_lsp_position(&mut self, byte_offset: u64) -> Result<Position, PositionError> {
        let bounds = self.line_for_byte(byte_offset)?;
        let content = self.line_content(bounds)?;
        let relative = byte_offset
            .checked_sub(bounds.start)
            .ok_or(PositionError::InvalidSnapshotIndex)?;
        let relative =
            usize::try_from(relative).map_err(|_| PositionError::ByteNotRepresentable)?;
        if relative > content.len() || !content.is_char_boundary(relative) {
            return Err(PositionError::ByteNotRepresentable);
        }

        let character = match self.encoding {
            SupportedPositionEncoding::Utf8 => {
                u32::try_from(relative).map_err(|_| PositionError::ByteNotRepresentable)?
            }
            SupportedPositionEncoding::Utf16 => {
                self.byte_to_encoded_units(content, relative, char::len_utf16)?
            }
            SupportedPositionEncoding::Utf32 => {
                self.byte_to_encoded_units(content, relative, |_| 1)?
            }
        };
        Ok(Position::new(bounds.line, character))
    }

    /// Validates an exact zero-based user byte insertion offset.
    ///
    /// Unlike server positions, this method never clamps. EOF is valid.
    ///
    /// # Errors
    ///
    /// Returns [`PositionError::InvalidUserPosition`] when the offset exceeds
    /// the snapshot length or splits a UTF-8 code point.
    pub fn validate_user_byte(&self, byte_offset: u64) -> Result<u64, PositionError> {
        let offset =
            usize::try_from(byte_offset).map_err(|_| PositionError::InvalidUserPosition)?;
        if offset > self.text.len() || !self.text.is_char_boundary(offset) {
            return Err(PositionError::InvalidUserPosition);
        }
        Ok(byte_offset)
    }

    /// Converts an exact zero-based user byte position to negotiated LSP units.
    ///
    /// This first applies [`Self::validate_user_byte`] and never clamps. A
    /// valid insertion offset inside a line terminator can still be rejected as
    /// [`PositionError::ByteNotRepresentable`] because LSP characters exclude
    /// terminators.
    ///
    /// # Errors
    ///
    /// Returns [`PositionError::InvalidUserPosition`] for an out-of-range or
    /// split-code-point offset, or the errors from [`Self::byte_to_lsp_position`].
    pub fn user_byte_to_lsp_position(
        &mut self,
        byte_offset: u64,
    ) -> Result<Position, PositionError> {
        self.validate_user_byte(byte_offset)?;
        self.byte_to_lsp_position(byte_offset)
    }

    /// Converts a one-based user line and scalar insertion column to bytes.
    ///
    /// Column 1 is the line start and the position immediately after the final
    /// scalar is also valid. Terminators are not columns. This method never
    /// clamps an oversized column.
    ///
    /// # Errors
    ///
    /// Returns [`PositionError::InvalidUserPosition`] when either coordinate
    /// is zero, the line does not exist, or the column is too large.
    pub fn user_line_scalar_to_byte(
        &mut self,
        line: u64,
        scalar_column: u64,
    ) -> Result<u64, PositionError> {
        if line == 0 || scalar_column == 0 {
            return Err(PositionError::InvalidUserPosition);
        }
        let zero_based_line = line
            .checked_sub(1)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(PositionError::InvalidUserPosition)?;
        let bounds = self
            .line_bounds(zero_based_line)
            .map_err(|_| PositionError::InvalidUserPosition)?;
        let content = self.line_content(bounds)?;
        let target = scalar_column - 1;
        let relative = self.scalar_units_to_byte_exact(content, target)?;
        bounds
            .start
            .checked_add(relative)
            .ok_or(PositionError::InvalidSnapshotIndex)
    }

    /// Converts an exact one-based user line/scalar column to negotiated LSP units.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::user_line_scalar_to_byte`] or
    /// [`Self::byte_to_lsp_position`].
    pub fn user_line_scalar_to_lsp_position(
        &mut self,
        line: u64,
        scalar_column: u64,
    ) -> Result<Position, PositionError> {
        let byte_offset = self.user_line_scalar_to_byte(line, scalar_column)?;
        self.byte_to_lsp_position(byte_offset)
    }

    /// Converts a snapshot byte offset to its one-based physical line and,
    /// when representable, its one-based Unicode-scalar column.
    ///
    /// Content offsets report the column of the offset itself; an offset at
    /// the content end reports the column just past the final scalar, which
    /// is the exclusive-end convention used by half-open ranges. Only offsets
    /// strictly beyond the content end — inside a multi-byte CR/LF terminator
    /// or at EOF after a final terminator — still report their physical line
    /// with a `None` column. Column scans charge the cumulative work budget
    /// one unit per examined scalar value.
    ///
    /// # Errors
    ///
    /// Returns [`PositionError::ByteNotRepresentable`] when the offset exceeds
    /// the snapshot length or splits a UTF-8 code point, or a work-limit error
    /// while counting units.
    pub fn byte_to_user_line_scalar(
        &mut self,
        byte_offset: u64,
    ) -> Result<(u64, Option<u64>), PositionError> {
        self.validate_user_byte(byte_offset)
            .map_err(|_| PositionError::ByteNotRepresentable)?;
        let line_count = self.line_index.line_count();
        if line_count == 0 {
            return Err(PositionError::ByteNotRepresentable);
        }

        let mut low = 1_u64;
        let mut high = line_count;
        while low < high {
            let middle = low + (high - low) / 2;
            let end = self
                .line_index
                .line_end(middle)
                .ok_or(PositionError::InvalidSnapshotIndex)?;
            if end <= byte_offset {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let candidate_end = self
            .line_index
            .line_end(low)
            .ok_or(PositionError::InvalidSnapshotIndex)?;
        // An offset at or past every line end can only be the content end of
        // an unterminated final line.
        let selected_line = if candidate_end <= byte_offset {
            line_count
        } else {
            low
        };
        let zero_based =
            u32::try_from(selected_line - 1).map_err(|_| PositionError::ByteNotRepresentable)?;
        let bounds = self.line_bounds(zero_based)?;
        let physical_end = self
            .line_index
            .line_end(selected_line)
            .ok_or(PositionError::InvalidSnapshotIndex)?;
        if byte_offset < bounds.start || byte_offset > physical_end {
            return Err(PositionError::ByteNotRepresentable);
        }
        let line = u64::from(zero_based) + 1;
        if byte_offset > bounds.content_end {
            // Strictly inside a multi-byte terminator, or EOF after a final
            // terminator: the physical line exists but has no scalar-column
            // position. Offsets exactly at the content end do have one: the
            // column just past the final scalar, matching half-open ranges.
            return Ok((line, None));
        }
        Ok((line, Some(self.scalar_column_before(bounds, byte_offset)?)))
    }

    fn line_bounds(&self, zero_based_line: u32) -> Result<LineBounds, PositionError> {
        let line = u64::from(zero_based_line)
            .checked_add(1)
            .ok_or(PositionError::NonexistentLine)?;
        let start = self
            .line_index
            .line_start(line)
            .ok_or(PositionError::NonexistentLine)?;
        let physical_end = self
            .line_index
            .line_end(line)
            .ok_or(PositionError::InvalidSnapshotIndex)?;
        let content_end = self.content_end(start, physical_end)?;
        Ok(LineBounds {
            line: zero_based_line,
            start,
            content_end,
        })
    }

    fn content_end(&self, start: u64, physical_end: u64) -> Result<u64, PositionError> {
        let start = usize::try_from(start).map_err(|_| PositionError::InvalidSnapshotIndex)?;
        let end = usize::try_from(physical_end).map_err(|_| PositionError::InvalidSnapshotIndex)?;
        let bytes = self
            .text
            .as_bytes()
            .get(start..end)
            .ok_or(PositionError::InvalidSnapshotIndex)?;
        let terminator_length = if bytes.ends_with(b"\r\n") {
            2
        } else if bytes.ends_with(b"\r") || bytes.ends_with(b"\n") {
            1
        } else {
            0
        };
        physical_end
            .checked_sub(terminator_length)
            .ok_or(PositionError::InvalidSnapshotIndex)
    }

    fn line_content(&self, bounds: LineBounds) -> Result<&'a str, PositionError> {
        let start =
            usize::try_from(bounds.start).map_err(|_| PositionError::InvalidSnapshotIndex)?;
        let end =
            usize::try_from(bounds.content_end).map_err(|_| PositionError::InvalidSnapshotIndex)?;
        self.text
            .get(start..end)
            .ok_or(PositionError::InvalidSnapshotIndex)
    }

    fn line_for_byte(&self, byte_offset: u64) -> Result<LineBounds, PositionError> {
        self.validate_user_byte(byte_offset)
            .map_err(|_| PositionError::ByteNotRepresentable)?;
        let line_count = self.line_index.line_count();
        if line_count == 0 {
            return Err(PositionError::ByteNotRepresentable);
        }

        let mut low = 1_u64;
        let mut high = line_count;
        while low < high {
            let middle = low + (high - low) / 2;
            let end = self
                .line_index
                .line_end(middle)
                .ok_or(PositionError::InvalidSnapshotIndex)?;
            if end < byte_offset {
                low = middle + 1;
            } else {
                high = middle;
            }
        }

        let candidate_end = self
            .line_index
            .line_end(low)
            .ok_or(PositionError::InvalidSnapshotIndex)?;
        let selected_line = if candidate_end == byte_offset && low < line_count {
            low + 1
        } else {
            low
        };
        let zero_based =
            u32::try_from(selected_line - 1).map_err(|_| PositionError::ByteNotRepresentable)?;
        let bounds = self.line_bounds(zero_based)?;
        if byte_offset < bounds.start || byte_offset > bounds.content_end {
            return Err(PositionError::ByteNotRepresentable);
        }
        Ok(bounds)
    }

    fn encoded_units_to_byte(
        &mut self,
        content: &str,
        character: u32,
        width: impl Fn(char) -> usize,
    ) -> Result<u64, PositionError> {
        let target = u64::from(character);
        let mut units = 0_u64;
        for (byte, scalar) in content.char_indices() {
            if units == target {
                return u64::try_from(byte).map_err(|_| PositionError::InvalidSnapshotIndex);
            }
            self.charge_code_point()?;
            let next = units
                .checked_add(
                    u64::try_from(width(scalar))
                        .map_err(|_| PositionError::InvalidSnapshotIndex)?,
                )
                .ok_or(PositionError::InvalidSnapshotIndex)?;
            if target < next {
                return Err(PositionError::CharacterSplitsCodeUnit);
            }
            units = next;
        }
        u64::try_from(content.len()).map_err(|_| PositionError::InvalidSnapshotIndex)
    }

    fn byte_to_encoded_units(
        &mut self,
        content: &str,
        byte_offset: usize,
        width: impl Fn(char) -> usize,
    ) -> Result<u32, PositionError> {
        let mut units = 0_u64;
        for (byte, scalar) in content.char_indices() {
            if byte == byte_offset {
                break;
            }
            self.charge_code_point()?;
            units = units
                .checked_add(
                    u64::try_from(width(scalar))
                        .map_err(|_| PositionError::InvalidSnapshotIndex)?,
                )
                .ok_or(PositionError::InvalidSnapshotIndex)?;
        }
        u32::try_from(units).map_err(|_| PositionError::ByteNotRepresentable)
    }

    fn scalar_units_to_byte_exact(
        &mut self,
        content: &str,
        target: u64,
    ) -> Result<u64, PositionError> {
        let mut scalars = 0_u64;
        for (byte, _) in content.char_indices() {
            if scalars == target {
                return u64::try_from(byte).map_err(|_| PositionError::InvalidSnapshotIndex);
            }
            self.charge_code_point()?;
            scalars = scalars
                .checked_add(1)
                .ok_or(PositionError::InvalidSnapshotIndex)?;
        }
        if scalars == target {
            u64::try_from(content.len()).map_err(|_| PositionError::InvalidSnapshotIndex)
        } else {
            Err(PositionError::InvalidUserPosition)
        }
    }

    fn scalar_column_before(
        &mut self,
        bounds: LineBounds,
        byte_offset: u64,
    ) -> Result<u64, PositionError> {
        let start =
            usize::try_from(bounds.start).map_err(|_| PositionError::InvalidSnapshotIndex)?;
        let relative = usize::try_from(
            byte_offset
                .checked_sub(bounds.start)
                .ok_or(PositionError::InvalidSnapshotIndex)?,
        )
        .map_err(|_| PositionError::ByteNotRepresentable)?;
        let prefix = self
            .text
            .get(
                start
                    ..start
                        .checked_add(relative)
                        .ok_or(PositionError::InvalidSnapshotIndex)?,
            )
            .ok_or(PositionError::InvalidSnapshotIndex)?;
        let mut scalars = 0_u64;
        for _ in prefix.chars() {
            self.charge_code_point()?;
            scalars = scalars
                .checked_add(1)
                .ok_or(PositionError::WorkLimitExceeded)?;
        }
        scalars
            .checked_add(1)
            .ok_or(PositionError::WorkLimitExceeded)
    }

    fn charge_code_point(&mut self) -> Result<(), PositionError> {
        let next = self
            .code_points_scanned
            .checked_add(1)
            .ok_or(PositionError::WorkLimitExceeded)?;
        if next > self.limits.maximum_code_points_scanned {
            return Err(PositionError::WorkLimitExceeded);
        }
        self.code_points_scanned = next;
        Ok(())
    }

    fn byte_length(&self) -> Result<u64, PositionError> {
        u64::try_from(self.text.len()).map_err(|_| PositionError::InvalidSnapshotIndex)
    }
}

fn utf8_units_to_byte(content: &str, character: u32) -> Result<u64, PositionError> {
    let requested = usize::try_from(character).map_err(|_| PositionError::InvalidSnapshotIndex)?;
    if requested >= content.len() {
        return u64::try_from(content.len()).map_err(|_| PositionError::InvalidSnapshotIndex);
    }
    if !content.is_char_boundary(requested) {
        return Err(PositionError::CharacterSplitsCodeUnit);
    }
    u64::try_from(requested).map_err(|_| PositionError::InvalidSnapshotIndex)
}

#[derive(Clone, Copy)]
struct LineBounds {
    line: u32,
    start: u64,
    content_end: u64,
}
