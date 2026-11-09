import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import * as api from "../api";
import { useArticleActions } from "../hooks/articleActions";
import { useFocusTrap } from "../hooks/useFocusTrap";
import { errorText } from "../lib/errors";
import { NO_AUTOCORRECT } from "../lib/inputProps";
import {
  catalogCategories,
  catalogHits,
  concretePath,
  extractParams,
  filterHits,
  paramsFromExample,
  parseCatalog,
  parseEnvJson,
  requiredEnv,
  type RsshubHit,
} from "../lib/rsshub";
import type { RsshubRouteRow } from "../types";
import ConfirmDialog from "./ConfirmDialog";
import Icon from "./Icon";

interface Props {
  onClose: () => void;
  onToast: (msg: string) => void;
}

/**
 * The RSSHub route marketplace. Two tabs:
 *
 *  - Market: the bundled route catalog, filtered client-side (search box,
 *    category dropdown, heat order, NSFW hidden by default) with a detail
 *    pane for filling the route's parameters and environment variables
 *    before installing / subscribing.
 *  - Installed: the route registry — per-route enable toggle, environment
 *    editor and uninstall.
 *
 * Routes run through the bundled local engine; nothing here touches a
 * hosted RSSHub instance.
 */
export default function RsshubMarketplaceDialog({ onClose, onToast }: Props) {
  const { t } = useTranslation();
  const dialogRef = useRef<HTMLDivElement>(null);
  useFocusTrap(dialogRef);

  const [tab, setTab] = useState<"market" | "installed">("market");

  // Escape closes the dialog from anywhere inside it.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal modal-explore"
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="rsshub-dialog-title"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="explore-header">
          <div className="explore-header-text">
            <h2 id="rsshub-dialog-title">{t("settings.rsshub.marketTitle")}</h2>
            <p className="explore-sub">{t("settings.rsshub.marketSub")}</p>
          </div>
          <div className="rsshub-tabs" role="tablist">
            <button
              role="tab"
              aria-selected={tab === "market"}
              className={`explore-chip${tab === "market" ? " active" : ""}`}
              onClick={() => setTab("market")}
            >
              {t("settings.rsshub.tabMarket")}
            </button>
            <button
              role="tab"
              aria-selected={tab === "installed"}
              className={`explore-chip${tab === "installed" ? " active" : ""}`}
              onClick={() => setTab("installed")}
            >
              {t("settings.rsshub.tabInstalled")}
            </button>
          </div>
          <button
            className="explore-close"
            onClick={onClose}
            aria-label={t("common.close")}
          >
            <Icon name="x" size={16} />
          </button>
        </div>
        {tab === "market" ? (
          <MarketTab onToast={onToast} />
        ) : (
          <InstalledTab onToast={onToast} />
        )}
      </div>
    </div>
  );
}

/* ── market tab ─────────────────────────────────────────────── */

function MarketTab({ onToast }: { onToast: (msg: string) => void }) {
  const { t } = useTranslation();
  const [search, setSearch] = useState("");
  const [category, setCategory] = useState<string | null>(null);
  const [hideNsfw, setHideNsfw] = useState(true);
  const [selected, setSelected] = useState<RsshubHit | null>(null);

  const catalog = useQuery({
    queryKey: ["rsshub-catalog"],
    queryFn: api.rsshubMarketplaceCatalog,
    // The catalog is a compile-time constant bundled into the binary — it
    // never goes stale within a session.
    staleTime: Infinity,
    select: parseCatalog,
  });
  const installed = useQuery({
    queryKey: ["rsshub-routes"],
    queryFn: api.rsshubRoutesInstalled,
  });

  const hits = useMemo(() => catalogHits(catalog.data ?? { generatedAt: "", namespaces: {} }), [catalog.data]);
  const filtered = useMemo(
    () => filterHits(hits, { query: search, category, hideNsfw }),
    [hits, search, category, hideNsfw],
  );
  const categories = useMemo(() => catalogCategories(catalog.data ?? { generatedAt: "", namespaces: {} }), [catalog.data]);

  const installedPaths = useMemo(
    () => new Set((installed.data ?? []).map((r) => r.routePath)),
    [installed.data],
  );

  // Virtualized route list — the catalog has thousands of routes.
  const scrollRef = useRef<HTMLDivElement>(null);
  const virt = useVirtualizer({
    count: filtered.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 64,
    overscan: 10,
  });

  return (
    <>
      <div className="explore-toolbar">
        <div className="explore-search">
          <Icon name="search" size={14} />
          <input
            type="text"
            placeholder={t("settings.rsshub.searchPlaceholder")}
            aria-label={t("settings.rsshub.searchPlaceholder")}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            {...NO_AUTOCORRECT}
          />
        </div>
        <select
          className="s-select"
          aria-label={t("settings.rsshub.category")}
          value={category ?? ""}
          onChange={(e) => setCategory(e.target.value || null)}
        >
          <option value="">{t("settings.rsshub.allCategories")}</option>
          {categories.map(({ tag, count }) => (
            <option key={tag} value={tag}>
              {`${tag} (${count})`}
            </option>
          ))}
        </select>
        <label className="rsshub-nsfw">
          <input
            type="checkbox"
            checked={hideNsfw}
            onChange={(e) => setHideNsfw(e.target.checked)}
          />
          {t("settings.rsshub.hideNsfw")}
        </label>
      </div>
      <div className="rsshub-body">
        <div className="rsshub-list" ref={scrollRef}>
          {catalog.isLoading && (
            <div className="discover-empty">{t("settings.rsshub.loading")}</div>
          )}
          {catalog.isSuccess && filtered.length === 0 && (
            <div className="discover-empty">
              {t("settings.rsshub.noResults")}
            </div>
          )}
          {catalog.isSuccess && filtered.length > 0 && (
            <div
              style={{
                height: virt.getTotalSize(),
                position: "relative",
                width: "100%",
              }}
            >
              {virt.getVirtualItems().map((vi) => {
                const hit = filtered[vi.index];
                const isInstalled = installedPaths.has(hit.routePath);
                const active = selected?.routePath === hit.routePath;
                return (
                  <button
                    key={vi.key}
                    ref={virt.measureElement}
                    data-index={vi.index}
                    className={`rsshub-row${active ? " active" : ""}`}
                    style={{
                      position: "absolute",
                      top: 0,
                      left: 0,
                      width: "100%",
                      transform: `translateY(${vi.start}px)`,
                    }}
                    onClick={() => setSelected(hit)}
                  >
                    <span className="rsshub-row-main">
                      <span className="rsshub-row-title">
                        {hit.route.name || hit.routePath}
                        {isInstalled && (
                          <Icon
                            name="check"
                            size={12}
                            color="var(--ok, #2E7D4F)"
                          />
                        )}
                      </span>
                      <span className="rsshub-row-path">{hit.routePath}</span>
                    </span>
                    <span className="rsshub-row-meta">
                      <span className="rsshub-ns">{hit.namespace}</span>
                      {requiredEnv(hit.route.features).length > 0 && (
                        <Icon
                          name="settings"
                          size={12}
                          color="var(--muted)"
                        />
                      )}
                      {hit.route.features.antiCrawler && (
                        <Icon name="info" size={12} color="var(--muted)" />
                      )}
                      {hit.route.features.requirePuppeteer && (
                        <span className="rsshub-unsupported">
                          {t("settings.rsshub.unsupported")}
                        </span>
                      )}
                    </span>
                  </button>
                );
              })}
            </div>
          )}
        </div>
        {selected && (
          <RouteDetail hit={selected} onToast={onToast} />
        )}
      </div>
    </>
  );
}

/** The detail pane: description, parameter form, environment form, and the
 *  install / install-and-subscribe / test actions. */
function RouteDetail({ hit, onToast }: { hit: RsshubHit; onToast: (msg: string) => void }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const actions = useArticleActions();

  const params = useMemo(() => extractParams(hit.routePath), [hit.routePath]);
  const [paramValues, setParamValues] = useState<Record<string, string>>({});
  const [envValues, setEnvValues] = useState<Record<string, string>>({});
  // Reset the forms when a different route is selected; the example URL
  // prefills the parameters it can.
  useEffect(() => {
    setParamValues(paramsFromExample(hit.routePath, hit.route.example));
    setEnvValues({});
  }, [hit.routePath, hit.route.example]);

  const installed = useQuery({
    queryKey: ["rsshub-routes"],
    queryFn: api.rsshubRoutesInstalled,
  });
  const feeds = useQuery({ queryKey: ["feeds"], queryFn: api.listFeeds });
  const isInstalled = (installed.data ?? []).some(
    (r) => r.routePath === hit.routePath,
  );

  const concrete = concretePath(hit.routePath, paramValues);
  const subscribeUrl = concrete ? `rsshub://${concrete}` : null;
  const alreadySubscribed =
    subscribeUrl != null &&
    (feeds.data ?? []).some((f) => f.feedUrl === subscribeUrl);
  const envItems = requiredEnv(hit.route.features);
  const unsupported = hit.route.features.requirePuppeteer;

  const install = useMutation({
    mutationFn: async (alsoSubscribe: boolean) => {
      if (!concrete) throw new Error("missing parameters");
      await api.rsshubRouteInstall(hit.routePath);
      if (envItems.length > 0) {
        await api.rsshubRouteSetEnv(hit.routePath, cleanEnv(envValues));
      }
      if (!alsoSubscribe) return null;
      return api.addFeed(`rsshub://${concrete}`, null);
    },
    onSuccess: (feed, alsoSubscribe) => {
      qc.invalidateQueries({ queryKey: ["rsshub-routes"] });
      if (alsoSubscribe && feed) {
        actions.refreshAfterBulk();
        onToast(t("settings.rsshub.subscribedToast", { title: feed.title }));
      } else {
        onToast(t("settings.rsshub.installedToast"));
      }
    },
    onError: (e) => onToast(errorText(e)),
  });

  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [testOk, setTestOk] = useState(false);
  const runTest = async () => {
    if (!concrete || testing) return;
    setTesting(true);
    setTestResult(null);
    try {
      const res = await api.rsshubTestRoute(concrete);
      setTestOk(true);
      setTestResult(t("settings.rsshub.testOk", { count: res.articleCount }));
    } catch (e) {
      setTestOk(false);
      setTestResult(errorText(e));
    } finally {
      setTesting(false);
    }
  };

  return (
    <aside className="rsshub-detail" aria-label={hit.route.name || hit.routePath}>
      <div className="rsshub-detail-head">
        <h3>{hit.route.name || hit.routePath}</h3>
        <span className="rsshub-ns">{hit.namespace}</span>
      </div>
      {hit.route.description && (
        <p className="rsshub-detail-desc">{hit.route.description}</p>
      )}
      {unsupported && (
        <p className="rsshub-detail-warn">
          <Icon name="alert" size={13} />
          {t("settings.rsshub.unsupportedDesc")}
        </p>
      )}
      {hit.route.features.antiCrawler && (
        <p className="rsshub-detail-note">
          <Icon name="info" size={13} />
          {t("settings.rsshub.antiCrawlerNote")}
        </p>
      )}

      {params.length > 0 && (
        <div className="rsshub-form">
          <h4>{t("settings.rsshub.paramsTitle")}</h4>
          {params.map((p) => (
            <label key={p.name} className="rsshub-field">
              <span className="rsshub-field-label">
                {p.name}
                {p.optional && (
                  <em className="rsshub-optional">
                    {t("settings.rsshub.optional")}
                  </em>
                )}
              </span>
              <input
                value={paramValues[p.name] ?? ""}
                onChange={(e) =>
                  setParamValues((v) => ({ ...v, [p.name]: e.target.value }))
                }
                placeholder={hit.route.parameters?.[p.name] ?? ""}
                {...NO_AUTOCORRECT}
              />
              {hit.route.parameters?.[p.name] && (
                <span className="rsshub-field-hint">
                  {hit.route.parameters[p.name]}
                </span>
              )}
            </label>
          ))}
        </div>
      )}

      {envItems.length > 0 && (
        <div className="rsshub-form">
          <h4>
            {t("settings.rsshub.envTitle")}
            <Icon name="settings" size={12} />
          </h4>
          {envItems.map((item) => (
            <label key={item.name} className="rsshub-field">
              <span className="rsshub-field-label">
                {item.name}
                {item.optional && (
                  <em className="rsshub-optional">
                    {t("settings.rsshub.optional")}
                  </em>
                )}
              </span>
              <input
                type="password"
                value={envValues[item.name] ?? ""}
                onChange={(e) =>
                  setEnvValues((v) => ({ ...v, [item.name]: e.target.value }))
                }
                placeholder={item.description ?? ""}
                {...NO_AUTOCORRECT}
              />
              {item.description && (
                <span className="rsshub-field-hint">{item.description}</span>
              )}
            </label>
          ))}
        </div>
      )}

      <div className="rsshub-actions">
        <button
          className="s-btn"
          disabled={unsupported || testing || !concrete}
          onClick={runTest}
        >
          {t(testing ? "settings.rsshub.testing" : "settings.rsshub.test")}
        </button>
        <button
          className="s-btn"
          disabled={unsupported || install.isPending}
          onClick={() => install.mutate(false)}
        >
          {isInstalled ? (
            <>
              <Icon name="check" size={12} /> {t("settings.rsshub.installed")}
            </>
          ) : (
            <>
              <Icon name="plus" size={12} /> {t("settings.rsshub.install")}
            </>
          )}
        </button>
        <button
          className="s-btn primary"
          disabled={unsupported || install.isPending || !concrete || alreadySubscribed}
          onClick={() => install.mutate(true)}
        >
          {alreadySubscribed ? (
            <>
              <Icon name="check" size={12} /> {t("settings.rsshub.subscribed")}
            </>
          ) : (
            <>
              <Icon name="rss" size={12} />
              {t("settings.rsshub.installAndSubscribe")}
            </>
          )}
        </button>
      </div>
      {!concrete && !unsupported && (
        <p className="rsshub-field-hint">
          {t("settings.rsshub.missingParams")}
        </p>
      )}
      {testResult && (
        <p
          className="rsshub-field-hint"
          style={{ color: testOk ? "var(--ok, #2E7D4F)" : "var(--danger, #CC2936)" }}
        >
          {testResult}
        </p>
      )}
      {subscribeUrl && (
        <p className="rsshub-field-hint rsshub-url">{subscribeUrl}</p>
      )}
    </aside>
  );
}

/** Strip blank keys/values so a half-filled form never persists noise. */
function cleanEnv(env: Record<string, string>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(env)) {
    if (k.trim() !== "" && v.trim() !== "") out[k.trim()] = v.trim();
  }
  return out;
}

/* ── installed tab ──────────────────────────────────────────── */

function InstalledTab({ onToast }: { onToast: (msg: string) => void }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const installed = useQuery({
    queryKey: ["rsshub-routes"],
    queryFn: api.rsshubRoutesInstalled,
  });
  const [editing, setEditing] = useState<RsshubRouteRow | null>(null);
  const [removing, setRemoving] = useState<RsshubRouteRow | null>(null);

  const toggle = useMutation({
    mutationFn: (r: RsshubRouteRow) =>
      api.rsshubRouteSetEnabled(r.routePath, !r.enabled),
    onSuccess: () =>
      qc.invalidateQueries({ queryKey: ["rsshub-routes"] }),
    onError: (e) => onToast(errorText(e)),
  });
  const remove = useMutation({
    mutationFn: (r: RsshubRouteRow) => api.rsshubRouteUninstall(r.routePath),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["rsshub-routes"] });
      onToast(t("settings.rsshub.uninstalledToast"));
    },
    onError: (e) => onToast(errorText(e)),
  });

  const rows = installed.data ?? [];

  return (
    <div className="rsshub-body">
      <div className="rsshub-list rsshub-installed">
        {installed.isLoading && (
          <div className="discover-empty">{t("settings.rsshub.loading")}</div>
        )}
        {installed.isSuccess && rows.length === 0 && (
          <div className="discover-empty">
            {t("settings.rsshub.noInstalled")}
          </div>
        )}
        {rows.map((r) => (
          <div key={r.routePath} className="rsshub-row static">
            <span className="rsshub-row-main">
              <span className="rsshub-row-title">{r.routePath}</span>
              <span className="rsshub-row-path">
                {t("settings.rsshub.installedAt", { date: r.installedAt.slice(0, 10) })}
              </span>
            </span>
            <span className="rsshub-row-meta">
              <button
                className="icon-btn"
                title={t("settings.rsshub.editEnv")}
                onClick={() => setEditing(r)}
              >
                <Icon name="settings" size={13} />
              </button>
              <button
                className="icon-btn"
                title={t("settings.rsshub.uninstall")}
                onClick={() => setRemoving(r)}
              >
                <Icon name="trash" size={13} />
              </button>
              <Toggle
                checked={r.enabled}
                onChange={() => toggle.mutate(r)}
                aria-label={`${r.routePath} — ${t("settings.rsshub.enabled")}`}
              />
            </span>
          </div>
        ))}
      </div>
      {editing && (
        <EnvEditorModal
          route={editing}
          onClose={() => setEditing(null)}
          onToast={onToast}
        />
      )}
      {removing && (
        <ConfirmDialog
          title={t("settings.rsshub.uninstall")}
          message={t("settings.rsshub.uninstallConfirm", {
            path: removing.routePath,
          })}
          confirmLabel={t("settings.rsshub.uninstall")}
          onConfirm={() => remove.mutate(removing)}
          onClose={() => setRemoving(null)}
        />
      )}
    </div>
  );
}

/** Small local toggle — the settings dialog owns a fancier one; this is the
 *  checkbox-styled minimal switch the installed list needs. */
function Toggle({
  checked,
  onChange,
  "aria-label": ariaLabel,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  "aria-label"?: string;
}) {
  return (
    <input
      type="checkbox"
      checked={checked}
      onChange={(e) => onChange(e.target.checked)}
      aria-label={ariaLabel}
      className="rsshub-toggle"
    />
  );
}

/** The per-route environment variable editor (installed tab's gear). Rows
 *  for the stored variables plus an add-row; saves the whole map. */
function EnvEditorModal({
  route,
  onClose,
  onToast,
}: {
  route: RsshubRouteRow;
  onClose: () => void;
  onToast: (msg: string) => void;
}) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const dialogRef = useRef<HTMLDivElement>(null);
  useFocusTrap(dialogRef);
  const [rows, setRows] = useState<{ key: string; value: string }[]>(
    Object.entries(parseEnvJson(route.envJson)).map(([key, value]) => ({
      key,
      value,
    })),
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const save = () => {
    const env: Record<string, string> = {};
    for (const { key, value } of rows) {
      if (key.trim() !== "" && value.trim() !== "") {
        env[key.trim()] = value.trim();
      }
    }
    api
      .rsshubRouteSetEnv(route.routePath, env)
      .then(() => {
        qc.invalidateQueries({ queryKey: ["rsshub-routes"] });
        onToast(t("settings.rsshub.envSaved"));
        onClose();
      })
      .catch((e) => onToast(errorText(e)));
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal"
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="rsshub-env-title"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id="rsshub-env-title">{t("settings.rsshub.editEnvTitle")}</h2>
        <p className="modal-hint">{t("settings.rsshub.editEnvHint")}</p>
        {rows.map((row, i) => (
          <div key={i} className="rsshub-env-row">
            <input
              value={row.key}
              placeholder="NAME"
              aria-label={t("settings.rsshub.envName")}
              onChange={(e) =>
                setRows((rs) =>
                  rs.map((r, j) => (j === i ? { ...r, key: e.target.value } : r)),
                )
              }
              spellCheck={false}
            />
            <input
              value={row.value}
              placeholder={t("settings.rsshub.envValue")}
              aria-label={t("settings.rsshub.envValue")}
              type="password"
              onChange={(e) =>
                setRows((rs) =>
                  rs.map((r, j) =>
                    j === i ? { ...r, value: e.target.value } : r,
                  ),
                )
              }
              spellCheck={false}
            />
            <button
              className="icon-btn"
              title={t("settings.rsshub.envRemove")}
              onClick={() => setRows((rs) => rs.filter((_, j) => j !== i))}
            >
              <Icon name="x" size={13} />
            </button>
          </div>
        ))}
        <button
          className="s-btn"
          onClick={() => setRows((rs) => [...rs, { key: "", value: "" }])}
        >
          <Icon name="plus" size={12} /> {t("settings.rsshub.envAdd")}
        </button>
        <div
          style={{
            display: "flex",
            gap: 8,
            justifyContent: "flex-end",
            marginTop: 16,
          }}
        >
          <button className="s-btn primary" onClick={save}>
            {t("common.save")}
          </button>
          <button className="s-btn" onClick={onClose}>
            {t("common.cancel")}
          </button>
        </div>
      </div>
    </div>
  );
}
