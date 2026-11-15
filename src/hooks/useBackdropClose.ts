// Backdrop-click dismissal for modal dialogs (`<div class="…-backdrop">`).
//
// A bare `onClick={onClose}` on the backdrop closes the dialog even when the
// click never started there: selecting text inside the dialog drags past the
// dialog's edge, and the browser fires the trailing `click` on the mousedown/
// mouseup targets' common ancestor — the backdrop itself. A pure text
// selection would then tear the dialog down mid-interaction.
//
// The close therefore requires all three of: `mousedown` on the backdrop,
// `mouseup` on the backdrop, and the trailing `click` landing there. A drag
// out of the dialog stays open (press inside), a drag that starts outside and
// releases inside the dialog stays open too (release inside), and a
// deliberate click on the backdrop still closes — the standard "both ends on
// the backdrop" contract.

import { useMemo, useRef } from "react";

/** Dialog-opening props for a backdrop element: spread them onto it. */
export function useBackdropClose(onClose: () => void) {
  const armed = useRef(false);
  const released = useRef(false);
  return useMemo(
    () => ({
      onMouseDown: (e: React.MouseEvent) => {
        armed.current = e.target === e.currentTarget;
        released.current = false;
      },
      onMouseUp: (e: React.MouseEvent) => {
        released.current = e.target === e.currentTarget;
      },
      onClick: (e: React.MouseEvent) => {
        const onBackdrop = e.target === e.currentTarget;
        if (onBackdrop && armed.current && released.current) onClose();
        armed.current = false;
        released.current = false;
      },
    }),
    [onClose],
  );
}
