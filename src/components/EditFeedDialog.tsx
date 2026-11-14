import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import * as api from "../api";
import { useArticleActions } from "../hooks/articleActions";
import { useFocusTrap } from "../hooks/useFocusTrap";
import { errorText } from "../lib/errors";
import { isComposingKey } from "../lib/ime";
import { NO_AUTOCORRECT } from "../lib/inputProps";
import type { Feed } from "../types";

interface Props {
  feed: Feed;
  onToast: (msg: string) => void;
  onClose: () => void;
}

/** The per-feed "open articles" options — identical to the per-feed rows in
 *  Settings → Subscriptions, reusing their labels (`default` = follow global). */
const OPEN_MODE_OPTIONS = [
  { value: "default", key: "settings.subscriptions.openDefault" },
  { value: "reader", key: "settings.subscriptions.openReader" },
  { value: "extracted", key: "settings.subscriptions.openExtracted" },
  { value: "web", key: "settings.subscriptions.openWeb" },
] as const;
type OpenModeChoice = (typeof OPEN_MODE_OPTIONS)[number]["value"];

/**
 * Edit one subscription: view/change its feed URL, plus the per-feed
 * auto-translate and open-mode settings that otherwise live only in Settings →
 * Subscriptions. The URL change is validated by the backend (fetch + parse,
 * like adding a feed) before anything is written; per-feed settings are plain
 * updates. Saving applies all changed fields, then closes.
 */
export default function EditFeedDialog({ feed, onToast, onClose }: Props) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const actions = useArticleActions();
  const dialogRef = useRef<HTMLDivElement>(null);
  useFocusTrap(dialogRef);

  const [url, setUrl] = useState(feed.feedUrl);
  const [autoTranslate, setAutoTranslate] = useState(feed.autoTranslate);
  const [openMode, setOpenMode] = useState<OpenModeChoice>(
    (feed.openMode as OpenModeChoice | null) ?? "default",
  );
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<unknown>(null);
  // Invalidate a pending save when this dialog instance unmounts: while a
  // save is in flight the user can still close (Escape/backdrop) and open the
  // dialog for another feed, and the old task's completion must not call the
  // now-new instance's onClose — it would close the new dialog and drop its
  // unsaved input. Cache refreshes still run regardless.
  const aliveRef = useRef(true);
  useEffect(() => {
    aliveRef.current = true;
    return () => {
      aliveRef.current = false;
    };
  }, []);

  // Escape closes the dialog regardless of focused control, with the IME
  // guard the other dialogs use (the URL field is a live CJK input target).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || isComposingKey(e) || e.defaultPrevented) return;
      e.preventDefault();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  // Parser-served (X), engine-routed (RSSHub) and newsletter feeds have URLs
  // produced by their own flows — the backend refuses to re-point them, so
  // the field is read-only here with a hint instead of a save-time error.
  const urlEditable = feed.sourceType === "rss";
  const urlChanged = urlEditable && url.trim() !== feed.feedUrl;
  const autoTranslateChanged = autoTranslate !== feed.autoTranslate;
  const openModeChanged = (feed.openMode ?? "default") !== openMode;
  const canSave = urlEditable ? url.trim() !== "" : true;

  const save = async () => {
    if (saving || !canSave) return;
    setSaving(true);
    setError(null);
    try {
      if (urlChanged) await api.editFeedUrl(feed.id, url.trim());
      if (autoTranslateChanged)
        await api.setFeedAutoTranslate(feed.id, autoTranslate);
      if (openModeChanged)
        await api.setFeedOpenMode(
          feed.id,
          openMode === "default" ? null : openMode,
        );
      if (urlChanged || autoTranslateChanged || openModeChanged) {
        // A URL edit brings new articles; a settings change only touches the
        // feed row. refreshAfterBulk covers both without over-invalidating.
        actions.refreshAfterBulk();
        qc.invalidateQueries({ queryKey: ["feeds"] });
        if (aliveRef.current) onToast(t("editFeed.toastSaved"));
      }
      if (aliveRef.current) onClose();
    } catch (e) {
      setError(e);
      setSaving(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal"
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="editfeed-dialog-title"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id="editfeed-dialog-title">{t("editFeed.title")}</h2>

        <p className="modal-hint" style={{ marginBottom: 6 }}>
          {urlEditable ? t("editFeed.urlHint") : t("editFeed.urlHintLocked")}
        </p>
        <input
          className="modal-input"
          type="text"
          autoFocus={urlEditable}
          readOnly={!urlEditable}
          value={url}
          placeholder={t("editFeed.urlLabel")}
          aria-label={t("editFeed.urlLabel")}
          {...NO_AUTOCORRECT}
          onChange={(e) => setUrl(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.nativeEvent.isComposing) save();
          }}
          style={urlEditable ? undefined : { opacity: 0.6 }}
        />

        {/* Per-feed auto-translate — same setting the Subscriptions panel's
            toggle drives, surfaced here so one dialog covers a feed's setup. */}
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 8,
            marginTop: 10,
          }}
        >
          <label htmlFor="editfeed-autotranslate">
            {t("editFeed.autoTranslate")}
          </label>
          <input
            id="editfeed-autotranslate"
            type="checkbox"
            className="s-toggle"
            checked={autoTranslate}
            onChange={(e) => setAutoTranslate(e.target.checked)}
          />
        </div>
        <p className="modal-hint" style={{ marginTop: 2 }}>
          {t("settings.subscriptions.autoTranslateDesc")}
        </p>

        {/* Per-feed open mode: how this feed's articles open in the reader. */}
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 8,
            marginTop: 10,
          }}
        >
          <label htmlFor="editfeed-openmode">{t("editFeed.openMode")}</label>
          <select
            id="editfeed-openmode"
            className="s-select"
            value={openMode}
            onChange={(e) => setOpenMode(e.target.value as OpenModeChoice)}
          >
            {OPEN_MODE_OPTIONS.map((o) => (
              <option key={o.value} value={o.value}>
                {t(o.key)}
              </option>
            ))}
          </select>
        </div>

        {error != null && <div className="modal-error">{errorText(error)}</div>}

        <div className="modal-actions">
          <button className="s-btn" onClick={onClose}>
            {t("common.cancel")}
          </button>
          <button
            className="s-btn primary"
            onClick={save}
            disabled={saving || !canSave}
          >
            {saving ? t("editFeed.saving") : t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}
