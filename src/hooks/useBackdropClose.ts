// Backdrop-click dismissal for modal dialogs (`<div class="…-backdrop">`).
//
// A bare `onClick={onClose}` on the backdrop closes the dialog even when the
// click never started there: selecting text inside the dialog drags past the
// dialog's edge, and the browser fires the trailing `click` on the mousedown/
// mouseup targets' common ancestor — the backdrop itself. A pure text
// selection would then tear the dialog down mid-interaction.
//
// The close therefore arms on `mousedown` hitting the backdrop itself and
// only fires when the `click` lands there too: a drag out of the dialog stays
// open, a deliberate click on the backdrop still closes, and a drag that
// starts outside (armed) but ends inside the dialog does not close either —
// the standard "both ends on the backdrop" contract.

import { useMemo, useRef } from "react";

/** Dialog-opening props for a backdrop element: spread them onto it. */
export function useBackdropClose(onClose: () => void) {
  const armed = useRef(false);
  return useMemo(
    () => ({
      onMouseDown: (e: React.MouseEvent) => {
        armed.current = e.target === e.currentTarget;
      },
      onClick: (e: React.MouseEvent) => {
        if (e.target === e.currentTarget && armed.current) onClose();
      },
    }),
    [onClose],
  );
}
