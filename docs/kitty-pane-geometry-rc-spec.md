# Kitty Pane Geometry RC Feature Spec

> **Status**: Design
> **Target**: kitty fork patch
> **Requester**: babel

## Executive Summary

Kitty's `ls` command returns pane dimensions in **characters** (`lines`, `columns`) but NOT **pixel coordinates**. Internally, kitty has full `WindowGeometry` data (`left`, `top`, `right`, `bottom` in pixels relative to OS window), but this isn't exposed via remote control.

This spec proposes exposing pane geometry through the RC protocol to enable precise pane positioning for overlays, visual sorting, and workspace restoration.

## Current State

### What `kitten @ ls` Returns (per pane)

```json
{
  "id": 6,
  "columns": 136,
  "lines": 30,
  "title": "~ — fish",
  "cwd": "/home/nuck",
  ...
}
```

### What Kitty Has Internally

```python
# kitty/types.py:51
class WindowGeometry(NamedTuple):
    left: int      # Pixel offset from OS window left edge
    top: int       # Pixel offset from OS window top edge
    right: int     # Pixel position of right edge
    bottom: int    # Pixel position of bottom edge
    xnum: int      # Column count (same as 'columns')
    ynum: int      # Line count (same as 'lines')
    spaces: Edges  # Margin/padding edges

# kitty/window.py:732
self.geometry: WindowGeometry = WindowGeometry(0, 0, 0, 0, 0, 0)
```

The gap: `as_dict()` at line 1946 only returns `lines`/`columns`, not the geometry.

## Use Cases from babel

### 1. Screen Position Sorting

**Current behavior** (src/daemon.rs:1989):
```rust
fn sort_by_screen_position<T, F>(items: &mut [T], get_platform_id: F)
```

Uses `xdotool getwindowgeometry` on `platform_window_id` to get **OS window** position. But when multiple panes exist in one OS window, they all get the same coordinates.

**Desired**: Sort panes by their actual visual position (left→right, top→bottom) even within the same OS window.

### 2. WSet Save/Restore (src/wset.rs)

**Current behavior**:
```rust
pub struct WindowGeometry {
    pub x: i32,      // OS window X
    pub y: i32,      // OS window Y
    pub width: u32,  // OS window width
    pub height: u32, // OS window height
}
```

Only captures OS window geometry via `xdotool`. Split layouts inside the window aren't persisted.

**Desired**: Save individual pane positions to restore exact split configurations.

### 3. Conversation Pager Overlay (docs/17-conversation-pager-spec.md)

The planned babel pager needs to draw overlays at exact pane locations. Without pixel coordinates, can't position overlays correctly.

### 4. Panel Plugin Layout Visualization

richmon/richspace plugins want to draw miniature representations of pane layouts. Needs proportional sizing data.

## Proposed Solution

### Option A: Extend `ls` Output (Recommended)

Add geometry fields to the existing `as_dict()` output:

```python
# kitty/window.py - in as_dict()
def as_dict(self, ...):
    g = self.geometry
    return {
        # ... existing fields ...
        'lines': self.screen.lines,
        'columns': self.screen.columns,

        # NEW: pixel geometry relative to OS window
        'geometry': {
            'left': g.left,
            'top': g.top,
            'right': g.right,
            'bottom': g.bottom,
            'width': g.right - g.left,
            'height': g.bottom - g.top,
        },
    }
```

**Pros**:
- Minimal change, extends existing command
- No new protocol surface
- Backward compatible (new field ignored by old clients)

**Cons**:
- Always included even when not needed (minor overhead)

### Option B: New `get-geometry` Command

New RC command: `kitten @ get-geometry --match id:123`

```json
{
  "window_id": 123,
  "os_window_id": 6,
  "geometry": {
    "left": 0,
    "top": 24,
    "right": 960,
    "bottom": 540,
    "width": 960,
    "height": 516
  },
  "screen_geometry": {
    "x": 1920,
    "y": 100,
    "width": 960,
    "height": 516
  }
}
```

**Pros**:
- Opt-in, no overhead when not needed
- Can compute absolute screen coords (combining OS window + pane offset)

**Cons**:
- New command to maintain
- Extra round-trip vs. getting it in `ls`

### Option C: Hybrid - `ls --geometry`

Add `--geometry` flag to `ls` command to include geometry in output:

```bash
kitten @ ls --geometry
```

**Pros**:
- Opt-in
- Uses existing command infrastructure

## Recommended Approach: Option A + Absolute Coords

Extend `ls` to include:

```json
{
  "id": 6,
  "columns": 136,
  "lines": 30,
  "geometry": {
    "left": 0,
    "top": 24,
    "right": 960,
    "bottom": 540
  },
  "screen": {
    "x": 1920,
    "y": 124,
    "width": 960,
    "height": 516
  }
}
```

Where:
- `geometry` = pixel coordinates relative to kitty OS window content area
- `screen` = absolute screen coordinates (for overlays, sorting)

Computing `screen` requires:
1. Get OS window position from X11/Wayland
2. Add pane's internal geometry offset

This should be computed in Python (not C) to avoid platform-specific code in the core.

## Implementation Path

### Phase 1: Expose Internal Geometry

1. Modify `window.py:as_dict()` to include `geometry` dict
2. Test with `kitten @ ls | jq`
3. Update TypedDict in fast_data_types.pyi

### Phase 2: Add Screen Coordinates

1. Add helper to get OS window screen position (already exists for some operations)
2. Compute `screen` by combining OS window pos + pane geometry
3. Handle multi-monitor coordinate systems

### Phase 3: Update babel

1. Update `RawWindow` struct to parse new geometry fields
2. Use `screen.x`, `screen.y` for sorting instead of xdotool calls
3. Store pane-level geometry in WSet for split restoration

## Backwards Compatibility

- New `geometry` field in `ls` output is additive
- Old clients ignore unknown fields
- No breaking changes to existing commands

## Testing

```bash
# Single pane
kitten @ ls | jq '.[0].tabs[0].windows[0].geometry'

# Splits
kitten @ launch --location=vsplit
kitten @ ls | jq '.[0].tabs[0].windows[].geometry'
# Should show left pane: {left: 0, right: ~960}
# Should show right pane: {left: ~960, right: ~1920}
```

## References

- kitty/types.py:51 - WindowGeometry NamedTuple
- kitty/window.py:732 - geometry storage
- kitty/window.py:947 - set_geometry()
- kitty/window.py:1946 - as_dict() to modify
- babel/src/kitty.rs - consumer code
- babel/src/daemon.rs:1989 - sort_by_screen_position
