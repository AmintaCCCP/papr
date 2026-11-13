// IME-composition guards for global keyboard handlers.
//
// CJK input methods turn physical keys into composition control keys: the
// Escape that cancels a candidate window, and the Enter that commits one,
// each surface in the webview as a real keydown. The codebase already guards
// Enter with `isComposing` at its submit sites (see AddFeedDialog,
// CommandPalette, PromptDialog); the dialog- and popover-level Escape
// handlers were missed, so dismissing the IME candidate window closed the
// dialog under the user's hands. Centralized here so every handler guards
// the same way.

/**
 * True when a keydown is part of IME composition (a candidate-window
 * control key, not a key the user meant for the app). `keyCode === 229`
 * covers older webviews that report the composition keycode without
 * setting `isComposing` on the event.
 */
export function isComposingKey(e: KeyboardEvent): boolean {
  return e.isComposing || e.keyCode === 229;
}
