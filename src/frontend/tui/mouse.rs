//! Mouse forwarding helpers for the container overlay (see WI-0088).
//!
//! When the agent running inside the container enables mouse tracking,
//! awman forwards scroll events to the agent's PTY instead of consuming
//! them for its own scrollback. Click and drag remain awman-side text
//! selection — those are handled in `handle_mouse_event` in `mod.rs`.

use crossterm::event::{MouseEvent, MouseEventKind};

use crate::frontend::tui::tabs::Tab;

/// Scroll awman's own container scrollback (the path used when the agent
/// has not enabled mouse tracking, or when Shift is held, or when the user
/// is already scrolled back). The focused slot's parser holds the overlay's
/// content and scrollback.
pub(super) fn handle_container_scroll(tab: &mut Tab, is_up: bool) {
    // ACP windows scroll their rendered update list, not a vt100 scrollback.
    if let Some(state) = tab.focused_slot().and_then(|s| s.acp_state().cloned()) {
        if let Ok(mut s) = state.lock() {
            let max = s.history.len();
            if is_up {
                s.scroll_offset = (s.scroll_offset + 3).min(max);
            } else {
                s.scroll_offset = s.scroll_offset.saturating_sub(3);
            }
        }
        return;
    }
    if is_up {
        let Some(slot) = tab.focused_slot_mut() else {
            return;
        };
        let max_scroll = {
            let screen = slot.vt100_parser.screen_mut();
            screen.set_scrollback(usize::MAX);
            let depth = screen.scrollback();
            screen.set_scrollback(0);
            depth
        };
        tab.container_scroll_offset = (tab.container_scroll_offset + 5).min(max_scroll);
    } else {
        tab.container_scroll_offset = tab.container_scroll_offset.saturating_sub(5);
    }
}

/// Forward a scroll event to the container's PTY stdin, translating
/// terminal coordinates to the agent's grid and encoding in the format
/// the agent expects. Events that land outside `container_inner_area`
/// (on the overlay border) are discarded.
pub(super) fn forward_mouse_scroll_to_pty(tab: &mut Tab, mouse: &MouseEvent) {
    let inner = match tab.container_inner_area {
        Some(r) => r,
        None => return,
    };

    if mouse.column < inner.x
        || mouse.row < inner.y
        || mouse.column >= inner.x + inner.width
        || mouse.row >= inner.y + inner.height
    {
        return;
    }

    let vt_col = mouse.column - inner.x;
    let vt_row = mouse.row - inner.y;

    // The focused slot owns the overlay — encode against its parser's
    // protocol and write to its stdin.
    let Some(slot) = tab.focused_slot() else {
        return;
    };
    let encoding = slot.vt100_parser.screen().mouse_protocol_encoding();

    if let Some(bytes) = encode_mouse_scroll(mouse.kind, vt_col, vt_row, encoding) {
        if let Some(ref tx) = slot.container_stdin_tx {
            let _ = tx.send(bytes);
        }
    }
}

/// Forward a scroll event as arrow-key presses for agents using
/// "alternate scroll" mode (DECSET 1007) — e.g. codex — which never enable
/// real mouse tracking and instead rely on the terminal translating wheel
/// events into cursor keys while the alternate screen is active. awman
/// plays the terminal's role here. Events that land outside
/// `container_inner_area` (on the overlay border) are discarded, matching
/// `forward_mouse_scroll_to_pty`.
pub(super) fn forward_alt_scroll_to_pty(tab: &mut Tab, mouse: &MouseEvent) {
    let inner = match tab.container_inner_area {
        Some(r) => r,
        None => return,
    };

    if mouse.column < inner.x
        || mouse.row < inner.y
        || mouse.column >= inner.x + inner.width
        || mouse.row >= inner.y + inner.height
    {
        return;
    }

    // Same focused-slot routing as `forward_mouse_scroll_to_pty`.
    let Some(slot) = tab.focused_slot() else {
        return;
    };
    let application_cursor = slot.vt100_parser.screen().application_cursor();

    if let Some(bytes) = encode_alt_scroll(mouse.kind, application_cursor) {
        if let Some(ref tx) = slot.container_stdin_tx {
            let _ = tx.send(bytes);
        }
    }
}

/// Encode a wheel tick as arrow-key presses for alternate-scroll mode:
/// three presses per tick (the de-facto terminal convention for wheel →
/// cursor-key translation), in application form (`ESC O A/B`) when DECCKM
/// is set, normal form (`ESC [ A/B`) otherwise.
pub(super) fn encode_alt_scroll(kind: MouseEventKind, application_cursor: bool) -> Option<Vec<u8>> {
    let arrow: &[u8] = match (kind, application_cursor) {
        (MouseEventKind::ScrollUp, false) => b"\x1b[A",
        (MouseEventKind::ScrollUp, true) => b"\x1bOA",
        (MouseEventKind::ScrollDown, false) => b"\x1b[B",
        (MouseEventKind::ScrollDown, true) => b"\x1bOB",
        _ => return None,
    };
    Some(arrow.repeat(3))
}

/// Encode a scroll event into the escape sequence the agent expects.
///
/// Mouse button codes: scroll-up = 64, scroll-down = 65 (X10 convention).
pub(super) fn encode_mouse_scroll(
    kind: MouseEventKind,
    col: u16,
    row: u16,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let button: u8 = match kind {
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        _ => return None,
    };

    match encoding {
        vt100::MouseProtocolEncoding::Default => {
            // X10-style: ESC [ M Cb Cx Cy — each coord offset by 32 and
            // emitted as a single byte. Clamp the u16 coord BEFORE the
            // `as u8` cast so values > 255 don't wrap-truncate; max byte
            // value 255 means each coord clamps at 222 (= 32 + 1 + 222).
            let cb = 32 + button;
            let cx = 32 + 1 + col.min(222) as u8;
            let cy = 32 + 1 + row.min(222) as u8;
            Some(vec![0x1b, b'[', b'M', cb, cx, cy])
        }
        vt100::MouseProtocolEncoding::Utf8 => {
            // UTF-8 extended (xterm mode 1005): coords encoded as UTF-8
            // scalars. Clamp at 2014 so cx/cy ≤ 2047 (U+07FF, the largest
            // 2-byte UTF-8 value) — matches xterm's documented range and
            // keeps us safely below the U+D800–U+DFFF surrogate gap.
            let cb = 32u32 + button as u32;
            let cx = 32u32 + 1 + (col as u32).min(2014);
            let cy = 32u32 + 1 + (row as u32).min(2014);
            let mut buf = vec![0x1b, b'[', b'M'];
            let mut tmp = [0u8; 4];
            // Clamping above guarantees valid scalars, so from_u32 cannot
            // return None here. The unwrap_or is defensive only.
            let s = char::from_u32(cb).unwrap_or(' ').encode_utf8(&mut tmp);
            buf.extend_from_slice(s.as_bytes());
            let s = char::from_u32(cx).unwrap_or(' ').encode_utf8(&mut tmp);
            buf.extend_from_slice(s.as_bytes());
            let s = char::from_u32(cy).unwrap_or(' ').encode_utf8(&mut tmp);
            buf.extend_from_slice(s.as_bytes());
            Some(buf)
        }
        vt100::MouseProtocolEncoding::Sgr => {
            // SGR: ESC [ < button ; col+1 ; row+1 M  (press) / m (release).
            // Scroll events are always press (M).
            Some(format!("\x1b[<{};{};{}M", button, col + 1, row + 1).into_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── encode_mouse_scroll: SGR encoding ─────────────────────────────────────

    #[test]
    fn encode_mouse_scroll_sgr_scroll_up() {
        // button=64 (scroll-up), col=4, row=2 → ESC[<64;5;3M
        let bytes = encode_mouse_scroll(
            MouseEventKind::ScrollUp,
            4,
            2,
            vt100::MouseProtocolEncoding::Sgr,
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[<64;5;3M");
    }

    #[test]
    fn encode_mouse_scroll_sgr_scroll_down() {
        // button=65 (scroll-down), col=4, row=2 → ESC[<65;5;3M
        let bytes = encode_mouse_scroll(
            MouseEventKind::ScrollDown,
            4,
            2,
            vt100::MouseProtocolEncoding::Sgr,
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[<65;5;3M");
    }

    // ── encode_mouse_scroll: Default (X10) encoding ───────────────────────────

    #[test]
    fn encode_mouse_scroll_default_scroll_up() {
        // ESC [ M Cb Cx Cy; Cb=32+64=96, Cx=32+1+4=37, Cy=32+1+2=35
        let bytes = encode_mouse_scroll(
            MouseEventKind::ScrollUp,
            4,
            2,
            vt100::MouseProtocolEncoding::Default,
        )
        .unwrap();
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 96, 37, 35]);
    }

    #[test]
    fn encode_mouse_scroll_default_scroll_down() {
        // Cb=32+65=97, same Cx/Cy as above
        let bytes = encode_mouse_scroll(
            MouseEventKind::ScrollDown,
            4,
            2,
            vt100::MouseProtocolEncoding::Default,
        )
        .unwrap();
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 97, 37, 35]);
    }

    #[test]
    fn encode_mouse_scroll_default_clamps_large_coords() {
        // col=300, row=500 — without clamping BEFORE the u8 cast, `col as u8`
        // would wrap-truncate (300 % 256 = 44) and silently produce wrong
        // coordinates. With the fix, each coord saturates at 222 → byte 255.
        let bytes = encode_mouse_scroll(
            MouseEventKind::ScrollUp,
            300,
            500,
            vt100::MouseProtocolEncoding::Default,
        )
        .unwrap();
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 96, 255, 255]);
    }

    // ── encode_mouse_scroll: UTF-8 encoding ───────────────────────────────────

    #[test]
    fn encode_mouse_scroll_utf8_scroll_up_small_coords() {
        // For small values UTF-8 and Default produce identical bytes; this
        // exercises the UTF-8 code path (char::from_u32 + encode_utf8).
        // cb=96 ('`'), cx=37 ('%'), cy=35 ('#') — all single-byte code points.
        let bytes = encode_mouse_scroll(
            MouseEventKind::ScrollUp,
            4,
            2,
            vt100::MouseProtocolEncoding::Utf8,
        )
        .unwrap();
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 0x60, 0x25, 0x23]);
    }

    #[test]
    fn encode_mouse_scroll_utf8_large_col_produces_multibyte() {
        // col=200 → cx = 32+1+200 = 233 = U+00E9 ('é') → UTF-8: [0xC3, 0xA9].
        // The Default encoding would emit a single byte 233 (0xE9) instead.
        let bytes = encode_mouse_scroll(
            MouseEventKind::ScrollUp,
            200,
            5,
            vt100::MouseProtocolEncoding::Utf8,
        )
        .unwrap();
        // cb=96, then cx='é' (2 bytes), then cy=38 '&'
        assert_eq!(bytes[0..3], [0x1b, b'[', b'M']);
        assert_eq!(bytes[3], 0x60, "cb should be 0x60 ('`')");
        assert_eq!(
            &bytes[4..6],
            &[0xC3, 0xA9],
            "cx=233 must encode as two UTF-8 bytes (U+00E9)"
        );
        assert_eq!(bytes[6], 0x26, "cy=38 should be '&'");
        assert_eq!(bytes.len(), 7);
    }

    #[test]
    fn encode_mouse_scroll_utf8_clamps_to_safe_range() {
        // u16::MAX would land cx/cy inside the U+D800-U+DFFF surrogate
        // range if not clamped; char::from_u32 returns None for surrogates.
        // With the clamp, cx/cy saturate at 2047 = U+07FF, the largest
        // 2-byte UTF-8 code point, encoded as [0xDF, 0xBF].
        let bytes = encode_mouse_scroll(
            MouseEventKind::ScrollUp,
            u16::MAX,
            u16::MAX,
            vt100::MouseProtocolEncoding::Utf8,
        )
        .unwrap();
        assert_eq!(bytes[0..3], [0x1b, b'[', b'M']);
        assert_eq!(bytes[3], 0x60, "cb single byte ('`')");
        assert_eq!(&bytes[4..6], &[0xDF, 0xBF], "cx must clamp to U+07FF");
        assert_eq!(&bytes[6..8], &[0xDF, 0xBF], "cy must clamp to U+07FF");
        assert_eq!(bytes.len(), 8);
    }

    #[test]
    fn encode_mouse_scroll_non_scroll_kind_returns_none() {
        let result = encode_mouse_scroll(
            MouseEventKind::Moved,
            0,
            0,
            vt100::MouseProtocolEncoding::Sgr,
        );
        assert!(result.is_none(), "non-scroll kind must return None");
    }

    // ── encode_alt_scroll (alternate scroll mode, DECSET 1007) ────────────────

    #[test]
    fn encode_alt_scroll_up_normal_cursor_keys() {
        let bytes = encode_alt_scroll(MouseEventKind::ScrollUp, false).unwrap();
        assert_eq!(bytes, b"\x1b[A\x1b[A\x1b[A");
    }

    #[test]
    fn encode_alt_scroll_down_normal_cursor_keys() {
        let bytes = encode_alt_scroll(MouseEventKind::ScrollDown, false).unwrap();
        assert_eq!(bytes, b"\x1b[B\x1b[B\x1b[B");
    }

    #[test]
    fn encode_alt_scroll_up_application_cursor_keys() {
        // DECCKM set → SS3-prefixed arrows.
        let bytes = encode_alt_scroll(MouseEventKind::ScrollUp, true).unwrap();
        assert_eq!(bytes, b"\x1bOA\x1bOA\x1bOA");
    }

    #[test]
    fn encode_alt_scroll_down_application_cursor_keys() {
        let bytes = encode_alt_scroll(MouseEventKind::ScrollDown, true).unwrap();
        assert_eq!(bytes, b"\x1bOB\x1bOB\x1bOB");
    }

    #[test]
    fn encode_alt_scroll_non_scroll_kind_returns_none() {
        assert!(encode_alt_scroll(MouseEventKind::Moved, false).is_none());
    }
}
