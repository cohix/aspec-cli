# Work Item: Feature

Title: Mouse Event Passthrough to Container Agents

## Summary

When an agent running inside the container enables mouse tracking (e.g. a TUI with scrollable panels), awman should forward **scroll events** to the agent's PTY instead of consuming them for its own scrollback. This lets users scroll within agent TUIs that have scrollable components — file listings, diff panels, log views, etc.

Click-and-drag events are **never forwarded** — they always perform awman's native text selection, ensuring users can always select and copy text from the container overlay.

Today all mouse scroll events are unconditionally captured by awman's `handle_mouse_event` for container scrollback navigation. Agents never receive mouse input.

## User Stories

### User Story 1
As a: developer running a TUI-based code agent (e.g. Claude Code interactive mode) inside an awman container

I want to: scroll a file-diff panel or code listing inside the agent's TUI using my mouse wheel

So I can: scroll within the agent's interface naturally without being limited to keyboard-only navigation.

### User Story 2
As a: developer reviewing agent output in the container overlay

I want to: scroll back through awman's scrollback history even when the agent has mouse tracking enabled

So I can: review earlier output without losing the ability to interact with the live agent TUI.

## Implementation Details

### 1. Detect agent mouse tracking via vt100

The vt100 parser already tracks whether the contained program has requested mouse events. `screen.mouse_protocol_mode()` returns `MouseProtocolMode::None` when the agent has not enabled mouse tracking, and a variant like `Press`, `PressRelease`, `ButtonMotion`, or `AnyMotion` when it has.

### 2. Conditional forwarding in `handle_mouse_event`

In `src/frontend/tui/mod.rs`, the `handle_mouse_event` function currently handles all mouse events for awman's own scrollback and text selection. Only **scroll events** are candidates for forwarding — click and drag events always stay with awman for text selection. Change the scroll event arms to:

```
MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
    // Workflow strip scroll takes priority (existing logic).
    ...

    let tab = app.active_tab_mut();
    if tab.container_window_state == ContainerWindowState::Maximized {
        let agent_wants_mouse = tab.vt100_parser.screen()
            .mouse_protocol_mode() != MouseProtocolMode::None;
        let at_live_view = tab.container_scroll_offset == 0;
        let shift_held = mouse.modifiers.contains(KeyModifiers::SHIFT);

        if agent_wants_mouse && at_live_view && !shift_held {
            // Forward scroll to agent PTY as encoded mouse escape sequence.
            forward_mouse_scroll_to_pty(tab, &mouse);
        } else {
            // Existing awman container scrollback handling.
            handle_scroll_for_awman(tab, &mouse);
        }
    } else {
        // Execution window scroll (non-maximized).
    }
}
```

The `shift_held` check gives the user an escape hatch: Shift+scroll always controls awman's scrollback even when the agent is tracking mouse events.

**Click and drag are never forwarded.** `MouseEventKind::Down`, `Drag`, and `Up` events always perform awman's native text selection regardless of the agent's mouse tracking state. This ensures the user can always select and copy text from the container overlay.

### 3. Encode mouse events for the PTY

Add a `forward_mouse_to_pty` function that encodes `crossterm::event::MouseEvent` into the format the agent expects, based on `screen.mouse_protocol_encoding()`:

- `MouseProtocolEncoding::Default` — X10-style single-byte encoding
- `MouseProtocolEncoding::Utf8` — UTF-8 extended encoding
- `MouseProtocolEncoding::Sgr` — SGR (`ESC[<...M` / `ESC[<...m`) encoding (most common in modern terminals)

Send the encoded bytes through `container_stdin_tx`.

### 4. Click and drag — always text selection

Do NOT forward `MouseEventKind::Down`, `MouseEventKind::Drag`, or `MouseEventKind::Up` to the PTY. These events always drive awman's native text selection, regardless of the agent's mouse tracking mode. This keeps text selection reliable and avoids ambiguity about whether a click targets the agent or awman.

### 5. Scrollback entry gesture

When the agent captures mouse scroll events, the user needs a way to enter awman's scrollback:

- **Shift+ScrollUp**: always enters/extends awman scrollback regardless of agent mouse tracking
- Once `container_scroll_offset > 0` (user is in scrollback), ALL scroll events go to awman until the user scrolls back to offset 0 (live view), at which point forwarding to the agent resumes
- This mirrors tmux's behavior: scroll enters copy-mode, reaching the bottom exits it

### 6. Coordinate translation

Mouse events from crossterm use absolute terminal coordinates. The agent expects coordinates relative to its PTY grid. Subtract `container_inner_area.x` and `container_inner_area.y` from the raw coordinates before encoding. Discard events that land outside the inner area (on the border).

## Edge Case Considerations

- **Agent enables mouse mid-session**: The vt100 parser updates `mouse_protocol_mode` in real time as it processes escape sequences, so the forwarding logic picks this up automatically.
- **Agent disables mouse**: Same — `mouse_protocol_mode` returns to `None` and awman resumes owning scroll events.
- **User is scrolled back when agent enables mouse**: Since `container_scroll_offset > 0`, awman keeps owning events. The user scrolls back to live view to resume agent interaction.
- **Scroll events on the overlay border**: Coordinate translation should discard scroll events outside `container_inner_area`.
- **Minimized/Hidden container**: The `container_maximized` guard prevents forwarding when the overlay isn't visible.
- **Alternate screen stripping (WI-0088 prerequisite)**: The current codebase strips alternate screen sequences from container output so scrollback accumulates on the primary grid. This is compatible — the agent's TUI renders on the primary grid, and mouse events are forwarded to the agent's stdin regardless of which grid the parser uses. The agent processes mouse input internally and emits cursor-positioning output that renders correctly on either grid.

## Test Considerations

- Unit test: `encode_mouse_event` encodes SGR scroll-up/down correctly
- Unit test: `encode_mouse_event` encodes Default (X10) scroll events correctly
- Unit test: `encode_mouse_event` encodes UTF-8 scroll events correctly
- Unit test: when `mouse_protocol_mode() == None`, scroll events go to awman scrollback (existing behavior preserved)
- Unit test: when `mouse_protocol_mode() != None` and `shift_held`, scroll goes to awman scrollback (escape hatch)
- Unit test: when `mouse_protocol_mode() != None` and `container_scroll_offset > 0`, scroll goes to awman scrollback (history browsing)
- Unit test: when `mouse_protocol_mode() != None` and at live view, scroll is forwarded to PTY
- Unit test: scroll outside `container_inner_area` is discarded, not forwarded
- Unit test: coordinate translation subtracts inner area origin
- Unit test: click/drag events always perform text selection, never forwarded to PTY (even when agent tracks mouse)

## Codebase Integration
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** — update the container overlay / TUI interaction docs to describe mouse forwarding behavior and the Shift+scroll escape hatch
- **Never create work-item-specific docs**
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
