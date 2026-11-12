import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { useFocusTrap } from "../hooks/useFocusTrap";
import { isComposingKey } from "../lib/ime";

interface Props {
  title: string;
  message: string;
  confirmLabel: string;
  /** Style the confirm button as destructive. On by default — this dialog
   *  exists for destructive confirmations. */
  danger?: boolean;
  onConfirm: () => void;
  onClose: () => void;
}

/**
 * A message-only confirmation modal — the themed replacement for the native
 * `window.confirm` on destructive actions. Mirrors PromptDialog's chrome.
 *
 * Cancel takes initial focus on purpose: an absent-minded Enter then backs
 * out of the action rather than carrying out the irreversible one.
 */
export default function ConfirmDialog({
  title,
  message,
  confirmLabel,
  danger = true,
  onConfirm,
  onClose,
}: Props) {
  const { t } = useTranslation();
  const dialogRef = useRef<HTMLDivElement>(null);
  useFocusTrap(dialogRef);

  // Escape must close regardless of which control holds focus. The
  // `isComposingKey` guard skips the Escape that only cancels an IME
  // candidate window — a stray close must not read as confirmation either.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || isComposingKey(e) || e.defaultPrevented) return;
      e.preventDefault();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const confirm = () => {
    onConfirm();
    onClose();
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal"
        ref={dialogRef}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="confirm-dialog-title"
        aria-describedby="confirm-dialog-message"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id="confirm-dialog-title">{title}</h2>
        <p id="confirm-dialog-message" className="modal-hint">
          {message}
        </p>
        <div className="modal-actions">
          <button className="s-btn" onClick={onClose} autoFocus>
            {t("common.cancel")}
          </button>
          <button
            className={`s-btn primary${danger ? " danger" : ""}`}
            onClick={confirm}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
