//! Unit tests for the pure pan-buffer fill logic. The panel reads these
//! control bits and silently misbehaves if any are wrong, so the byte
//! layout is checked against the SWTCON wire format in detail.

use swtcon::constants::{PAN_BUFFER_SIZE, PAN_LINE_SIZE, SCREEN_WIDTH};
use swtcon::fb::{fill_first_line, fill_line, fill_pan_buffer};

const CELLS_PER_LINE: usize = PAN_LINE_SIZE / 4;

#[test]
fn first_line_base_pattern() {
    let mut line = vec![0u32; CELLS_PER_LINE];
    fill_first_line(&mut line);
    // Cells outside any overlay region just have the base value.
    assert_eq!(line[0], 0x0043_0000, "cell 0 base");
    assert_eq!(line[15], 0x0043_0000, "cell 15 (before any overlay)");
    assert_eq!(line[19], 0x0043_0000, "cell 19 (just before 0x40000 range)");
    assert_eq!(line[143], 0x0043_0000, "cell 143 (just after 0x40000 range)");
    assert_eq!(line[CELLS_PER_LINE - 1], 0x0043_0000, "last cell");
}

#[test]
fn first_line_or_overlay() {
    let mut line = vec![0u32; CELLS_PER_LINE];
    fill_first_line(&mut line);
    // 0x40000 OR'd into [20..143).
    for i in 20..143 {
        assert_eq!(line[i] & 0x0004_0000, 0x0004_0000, "cell {i} should have 0x40000");
    }
    // Outside [20..143) the bit must NOT be set.
    assert_eq!(line[19] & 0x0004_0000, 0);
    assert_eq!(line[143] & 0x0004_0000, 0);
}

#[test]
fn first_line_clear_mask() {
    let mut line = vec![0u32; CELLS_PER_LINE];
    fill_first_line(&mut line);
    // [40..103) gets cleared with 0xfffdffff (bit 17 cleared).
    for i in 40..103 {
        assert_eq!(line[i] & 0x0002_0000, 0,
                   "cell {i} should have bit 17 cleared");
    }
}

#[test]
fn fill_line_no_value() {
    let mut line = vec![0u32; CELLS_PER_LINE];
    fill_line(&mut line, None);
    assert_eq!(line[0], 0x0041_0000, "cell 0 base");
    // 0x200000 OR'd into [8..8+0xb) = [8..19).
    for i in 8..19 {
        assert_eq!(line[i] & 0x0020_0000, 0x0020_0000, "cell {i} +0x200000");
    }
    // 0x20000 OR'd into [0x37..0x37+200) = [55..255).
    for i in 55..255 {
        assert_eq!(line[i] & 0x0002_0000, 0x0002_0000, "cell {i} +0x20000");
    }
    // No value: the [0x1a..0x1a+0xea) range should NOT have 0x100000 set
    // and should not have any pixel-data bits in low 16.
    for i in 0x1a..0x1a + 0xea {
        assert_eq!(line[i] & 0x0010_0000, 0, "cell {i} no 0x100000");
        assert_eq!(line[i] & 0x0000_ffff, 0, "cell {i} no pixel data");
    }
}

#[test]
fn fill_line_with_value() {
    let mut line = vec![0u32; CELLS_PER_LINE];
    fill_line(&mut line, Some(0xABCD));
    // 0x100000 OR'd into [0x1a..0x1a+0xea).
    for i in 0x1a..0x1a + 0xea {
        assert_eq!(line[i] & 0x0010_0000, 0x0010_0000, "cell {i} +0x100000");
        assert_eq!(line[i] & 0x0000_ffff, 0xABCD, "cell {i} pixel = 0xABCD");
    }
    // Just before the value range, no value bits should be set.
    assert_eq!(line[0x19] & 0x0000_ffff, 0);
    // The range [0x1a..0x1a+0xea) = [26..260) covers cells right up to the
    // end of the line (CELLS_PER_LINE == 260), so there's no "after" cell
    // to check on the same line.
}

#[test]
fn pan_buffer_layout() {
    let mut buf = vec![0u8; PAN_BUFFER_SIZE * PAN_LINE_SIZE];
    fill_pan_buffer(&mut buf, 0);

    let cells: &[u32] = unsafe {
        core::slice::from_raw_parts(buf.as_ptr() as *const u32, buf.len() / 4)
    };

    // Row 0 should match fill_first_line output.
    let mut expected_row0 = vec![0u32; CELLS_PER_LINE];
    fill_first_line(&mut expected_row0);
    assert_eq!(&cells[0..CELLS_PER_LINE], &expected_row0[..], "row 0");

    // Rows 1, 2 should match fill_line(None).
    let mut expected_preamble = vec![0u32; CELLS_PER_LINE];
    fill_line(&mut expected_preamble, None);
    assert_eq!(&cells[CELLS_PER_LINE..2 * CELLS_PER_LINE], &expected_preamble[..], "row 1");
    assert_eq!(&cells[2 * CELLS_PER_LINE..3 * CELLS_PER_LINE], &expected_preamble[..], "row 2");

    // Row 3 should match fill_line(Some(0)) — the content template.
    let mut expected_template = vec![0u32; CELLS_PER_LINE];
    fill_line(&mut expected_template, Some(0));
    assert_eq!(&cells[3 * CELLS_PER_LINE..4 * CELLS_PER_LINE], &expected_template[..], "row 3 template");

    // Rows 4..(4+SCREEN_WIDTH): each should be a copy of row 3.
    for line_idx in 0..SCREEN_WIDTH {
        let row_start = (4 + line_idx) * CELLS_PER_LINE;
        assert_eq!(
            &cells[row_start..row_start + CELLS_PER_LINE],
            &expected_template[..],
            "row {} should be a copy of row 3", 4 + line_idx
        );
    }
}

#[test]
fn pan_buffer_value_propagates() {
    let mut buf = vec![0u8; PAN_BUFFER_SIZE * PAN_LINE_SIZE];
    fill_pan_buffer(&mut buf, 0xDEAD);

    let cells: &[u32] = unsafe {
        core::slice::from_raw_parts(buf.as_ptr() as *const u32, buf.len() / 4)
    };

    // Spot-check: in any content row, the value-overlay region [0x1a..0x1a+0xea)
    // should have 0xDEAD in the low 16 bits.
    let row3_start = 3 * CELLS_PER_LINE;
    assert_eq!(cells[row3_start + 0x1a] & 0xffff, 0xDEAD);
    let row99_start = (4 + 99) * CELLS_PER_LINE;
    assert_eq!(cells[row99_start + 0x80] & 0xffff, 0xDEAD);
}
