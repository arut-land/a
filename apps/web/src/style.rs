//! Inline styles: the UI has no stylesheet, so each element carries its own.
//! Colors are custom properties set on the root and follow the system's
//! light or dark scheme through `light-dark()`; layouts are fluid, so no
//! media queries are needed.

pub const ROOT: &str = "color-scheme: light dark; \
    --bg: light-dark(#f6f6f7, #121214); \
    --surface: light-dark(#ffffff, #1c1c1f); \
    --sunken: light-dark(#f0f0f2, #17171a); \
    --line: light-dark(#e2e2e6, #2e2e33); \
    --text: light-dark(#1a1a1e, #e8e8ec); \
    --muted: light-dark(#6c6c75, #9b9ba5); \
    --accent: light-dark(#3157e0, #8aa4ff); \
    --on-accent: light-dark(#ffffff, #10132a); \
    --mine: light-dark(#e7ecfd, #252f55); \
    --ok: light-dark(#1d7a3d, #7fd99c); \
    --bad: light-dark(#b42318, #ff9a8f); \
    --warn: light-dark(#8a5a00, #f0c470); \
    background: var(--bg); color: var(--text); \
    font: 15px/1.5 system-ui, -apple-system, 'Segoe UI', sans-serif;";

pub const BODY: &str = "margin: 0; height: 100dvh; display: flex; flex-direction: column;";

pub const BAR: &str = "display: flex; align-items: center; gap: 0.75rem; flex-wrap: wrap; \
    padding: 0.6rem clamp(0.75rem, 3vw, 1.5rem); background: var(--surface); \
    border-bottom: 1px solid var(--line);";

pub const BRAND: &str = "font-weight: 650; color: var(--text); text-decoration: none;";

pub const DIM: &str = "color: var(--muted); font-size: 0.85em; overflow-wrap: anywhere;";

pub const MAIN: &str = "flex: 1; overflow-y: auto; padding: clamp(0.75rem, 3vw, 1.5rem);";

pub const COLUMN: &str = "width: min(46rem, 100%); margin: 0 auto; display: flex; \
    flex-direction: column; gap: 0.75rem;";

/// Lays the transcript out from the bottom, so the view stays pinned to the
/// newest item as it grows, without a script to scroll it.
pub const SCROLLER: &str = "flex: 1; overflow-y: auto; display: flex; \
    flex-direction: column-reverse; padding: clamp(0.75rem, 3vw, 1.5rem);";

pub const CARD: &str = "display: flex; flex-wrap: wrap; align-items: baseline; gap: 0.25rem 1rem; \
    padding: 0.75rem 1rem; background: var(--surface); border: 1px solid var(--line); \
    border-radius: 10px; color: var(--text); text-decoration: none;";

pub const MONO: &str = "font-family: ui-monospace, 'SF Mono', Menlo, monospace; font-size: 0.9em;";

pub const BUTTON: &str = "font: inherit; padding: 0.45rem 1rem; border-radius: 8px; \
    border: 1px solid var(--accent); background: var(--accent); color: var(--on-accent); \
    cursor: pointer;";

pub const BUTTON_QUIET: &str = "font: inherit; padding: 0.45rem 1rem; border-radius: 8px; \
    border: 1px solid var(--line); background: var(--surface); color: var(--text); \
    cursor: pointer;";

pub const COMPOSER: &str = "display: flex; gap: 0.5rem; width: min(46rem, 100%); \
    margin: 0 auto; box-sizing: border-box;";

pub const COMPOSER_BAR: &str = "padding: 0.75rem clamp(0.75rem, 3vw, 1.5rem); \
    background: var(--surface); border-top: 1px solid var(--line);";

pub const INPUT: &str = "flex: 1; min-width: 0; font: inherit; padding: 0.5rem 0.75rem; \
    border-radius: 8px; border: 1px solid var(--line); background: var(--bg); \
    color: var(--text);";

pub const MINE: &str = "align-self: flex-end; max-width: min(36rem, 85%); \
    padding: 0.5rem 0.85rem; border-radius: 12px 12px 4px 12px; background: var(--mine); \
    white-space: pre-wrap; overflow-wrap: anywhere;";

pub const THEIRS: &str = "align-self: flex-start; max-width: min(40rem, 92%); \
    padding: 0.5rem 0.85rem; border-radius: 12px 12px 12px 4px; background: var(--surface); \
    border: 1px solid var(--line); white-space: pre-wrap; overflow-wrap: anywhere;";

pub const TOOL: &str = "align-self: stretch; border: 1px solid var(--line); border-radius: 10px; \
    background: var(--surface); overflow: hidden;";

pub const TOOL_HEAD: &str = "display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem; \
    padding: 0.5rem 0.85rem;";

pub const OUTPUT: &str = "margin: 0; padding: 0.5rem 0.85rem; background: var(--sunken); \
    border-top: 1px solid var(--line); white-space: pre-wrap; overflow-wrap: anywhere; \
    max-height: 18rem; overflow-y: auto; font-family: ui-monospace, 'SF Mono', Menlo, monospace; \
    font-size: 0.85em;";

pub const ASK: &str = "display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem; \
    padding: 0.5rem 0.85rem; border-top: 1px solid var(--line); background: var(--sunken);";

pub const NOTE: &str = "margin: 0; align-self: center; color: var(--muted); font-size: 0.85em; \
    text-align: center; overflow-wrap: anywhere;";

pub const OK: &str = "margin: 0; color: var(--ok); font-size: 0.85em;";
pub const BAD: &str = "margin: 0; color: var(--bad); font-size: 0.85em; white-space: pre-wrap;";
pub const WARN: &str = "margin: 0; color: var(--warn); font-size: 0.85em;";

pub const RECAP: &str = "align-self: stretch; padding: 0.5rem 0.85rem; border-radius: 10px; \
    border: 1px dashed var(--line); color: var(--muted); font-size: 0.85em;";

pub const LINK: &str = "color: var(--accent);";
