// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Byte offset → (line, column) for diagnostic rendering.

/// 1-based `(line, column)` of byte `offset` in `src`; `\n`, `\r\n` and a lone `\r` each end a
/// line; the column counts Unicode scalar values. An offset past the end clamps to the end.
#[must_use]
pub fn line_col(src: &str, offset: u32) -> (u32, u32) {
    let b = src.as_bytes();
    let end = (offset as usize).min(b.len());
    let mut line = 1u32;
    let mut line_start = 0usize;
    let mut i = 0usize;
    while i < end {
        match b[i] {
            b'\n' => {
                line += 1;
                line_start = i + 1;
            }
            b'\r' => {
                if i + 1 < end && b[i + 1] == b'\n' {
                    i += 1;
                }
                line += 1;
                line_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let mut col = 1u32;
    let mut j = line_start;
    while j < end {
        if b[j] & 0xC0 != 0x80 {
            col += 1;
        }
        j += 1;
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::line_col;
    #[test]
    fn line_col_counts_all_terminators() {
        let s = "ab\ncd\r\nef\rgh";
        assert_eq!(line_col(s, 0), (1, 1));
        assert_eq!(line_col(s, 3), (2, 1));
        assert_eq!(line_col(s, 7), (3, 1));
        assert_eq!(line_col(s, 10), (4, 1));
        assert_eq!(line_col(s, 11), (4, 2));
        assert_eq!(line_col("é=1", 2), (1, 2));
    }
}
