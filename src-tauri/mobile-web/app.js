(function () {
  "use strict";

  const I18N = {
    zh: {
      unread: "未读",
      all: "全部文章",
      starred: "星标收藏",
      readLater: "稍后读",
      folders: "分类文件夹",
      feeds: "订阅源",
      searchPlaceholder: "搜索文章…",
      empty: "暂无文章",
      loadMore: "加载更多",
      noIntegrations: "未在电脑端配置任何启用的集成",
      shareTitle: "分享到集成",
      cancel: "取消",
      translate: "翻译",
      original: "原文",
      translating: "翻译中…",
      summarize: "总结",
      summarizing: "总结中…",
      aiSummary: "AI 总结",
      star: "收藏",
      starredBtn: "已收藏",
      copy: "复制链接",
      copied: "已复制文章链接",
      copyFail: "复制失败",
      open: "浏览器",
      share: "集成",
      openApp: "点击打开应用",
      noUrl: "该文章没有原文链接",
      markAllConfirm: "将当前分类全部标为已读？",
      markedAllRead: "已全部标为已读",
      synced: "已同步",
      sentTo: "已发送到 {{name}}",
      fulltextDone: "已提取全文",
      translatedBadge: "已翻译",
    },
    en: {
      unread: "Unread",
      all: "All Articles",
      starred: "Starred",
      readLater: "Read Later",
      folders: "Folders",
      feeds: "Feeds",
      searchPlaceholder: "Search articles…",
      empty: "No articles",
      loadMore: "Load more",
      noIntegrations: "No enabled integrations configured on desktop",
      shareTitle: "Share to Integration",
      cancel: "Cancel",
      translate: "Translate",
      original: "Original",
      translating: "Translating…",
      summarize: "Summary",
      summarizing: "Summarizing…",
      aiSummary: "AI Summary",
      star: "Star",
      starredBtn: "Starred",
      copy: "Copy Link",
      copied: "Article link copied",
      copyFail: "Copy failed",
      open: "Browser",
      share: "Share",
      openApp: "Open App",
      noUrl: "Article has no URL",
      markAllConfirm: "Mark all articles in this view as read?",
      markedAllRead: "Marked all as read",
      synced: "Synced",
      sentTo: "Sent to {{name}}",
      fulltextDone: "Full text extracted",
      translatedBadge: "Translated",
    },
    ja: {
      unread: "未読",
      all: "すべて",
      starred: "スター付き",
      readLater: "あとで読む",
      folders: "フォルダ",
      feeds: "フィード",
      searchPlaceholder: "記事を検索…",
      empty: "記事がありません",
      loadMore: "もっと読み込む",
      noIntegrations: "PC側で有効な連携が設定されていません",
      shareTitle: "連携サービスへ共有",
      cancel: "キャンセル",
      translate: "翻訳",
      original: "原文",
      translating: "翻訳中…",
      summarize: "要約",
      summarizing: "要約中…",
      aiSummary: "AI 要約",
      star: "お気に入り",
      starredBtn: "保存済み",
      copy: "リンクをコピー",
      copied: "リンクをコピーしました",
      copyFail: "コピーに失敗しました",
      open: "ブラウザ",
      share: "共有",
      openApp: "アプリを開く",
      noUrl: "記事のURLがありません",
      markAllConfirm: "この一覧をすべて既読にしますか？",
      markedAllRead: "すべて既読にしました",
      synced: "同期しました",
      sentTo: "{{name}} に送信しました",
      fulltextDone: "全文を取得しました",
      translatedBadge: "翻訳済み",
    },
  };

  const BUILTIN_NAMES = {
    zh: {
      cubox: "Cubox",
      readwise: "Readwise",
      instapaper: "Instapaper",
      outline: "Outline",
      readeck: "Readeck",
      zotero: "Zotero",
      obsidian: "Obsidian",
    },
    en: {
      cubox: "Cubox",
      readwise: "Readwise",
      instapaper: "Instapaper",
      outline: "Outline",
      readeck: "Readeck",
      zotero: "Zotero",
      obsidian: "Obsidian",
    },
    ja: {
      cubox: "Cubox",
      readwise: "Readwise",
      instapaper: "Instapaper",
      outline: "Outline",
      readeck: "Readeck",
      zotero: "Zotero",
      obsidian: "Obsidian",
    },
  };

  const state = {
    lang: "zh",
    folders: [],
    feeds: [],
    counts: { unread: 0, starred: 0, readLater: 0 },
    integrations: null,
    translateEngine: "llm",
    translateTargetLang: "zh",
    query: { kind: "unread", value: null, label: "" },
    unreadOnly: false,
    search: "",
    articles: [],
    hasMore: false,
    loadingList: false,
    currentArticle: null,
    showTranslation: false,
    translating: false,
    summarizing: false,
  };

  function tr(key, vars) {
    const dict = I18N[state.lang] || I18N.zh;
    let s = dict[key] || I18N.en[key] || key;
    if (vars) {
      for (const [k, v] of Object.entries(vars)) {
        s = s.replace(`{{${k}}}`, String(v));
      }
    }
    return s;
  }

  function $(id) {
    return document.getElementById(id);
  }

  let toastTimer = null;
  function showToast(msg, actionHref, actionLabel) {
    const el = $("toast");
    el.innerHTML = "";
    const span = document.createElement("span");
    span.textContent = msg;
    el.appendChild(span);
    if (actionHref && actionLabel) {
      const a = document.createElement("a");
      a.href = actionHref;
      a.textContent = ` ${actionLabel}`;
      a.style.color = "#ffd2bd";
      a.style.textDecoration = "underline";
      a.style.marginLeft = "6px";
      el.appendChild(a);
    }
    el.classList.remove("hidden");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.add("hidden"), actionHref ? 5000 : 2400);
  }

  const TOKEN_KEY = "papr_mobile_token";
  function getToken() {
    return localStorage.getItem(TOKEN_KEY) || "";
  }
  function setToken(t) {
    if (t) localStorage.setItem(TOKEN_KEY, t);
    else localStorage.removeItem(TOKEN_KEY);
  }

  async function apiFetch(url, options = {}) {
    const tok = getToken();
    const headers = {
      "Content-Type": "application/json",
      ...(options.headers || {}),
    };
    if (tok) {
      headers.Authorization = `Bearer ${tok}`;
    }
    const res = await fetch(url, {
      ...options,
      headers,
    });
    if (res.status === 401) {
      const entered = window.prompt(
        state.lang === "en"
          ? "Enter pairing token from Papr Settings:"
          : state.lang === "ja"
          ? "Papr 設定のペアリングトークンを入力してください:"
          : "请输入电脑端「设置 → 同步」链接中的 token："
      );
      if (entered && entered.trim()) {
        setToken(entered.trim());
        await fetch("/api/auth", {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            Authorization: `Bearer ${entered.trim()}`,
          },
          body: "{}",
        }).catch(() => {});
        return apiFetch(url, options);
      }
    }
    const text = await res.text();
    let data = null;
    try {
      data = text ? JSON.parse(text) : null;
    } catch {
      data = null;
    }
    if (!res.ok) {
      throw new Error((data && data.error) || text || `HTTP ${res.status}`);
    }
    return data;
  }

  function escapeHtml(s) {
    return String(s || "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function relTime(iso) {
    if (!iso) return "";
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return "";
    const mins = Math.floor((Date.now() - d.getTime()) / 60000);
    if (mins < 1) return "now";
    if (mins < 60) return `${mins}m`;
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return `${hrs}h`;
    const days = Math.floor(hrs / 24);
    if (days < 30) return `${days}d`;
    return d.toLocaleDateString();
  }

  function proxyImageUrl(url, pageUrl) {
    if (!url || !/^https?:\/\//i.test(url)) return url;
    const p = new URLSearchParams({ url });
    if (pageUrl) p.set("pageUrl", pageUrl);
    return `/api/image?${p.toString()}`;
  }

  function rewriteHtmlImages(html, pageUrl) {
    if (!html) return "";
    const tpl = document.createElement("template");
    tpl.innerHTML = html;
    tpl.content.querySelectorAll("img").forEach((img) => {
      const src = img.getAttribute("src");
      if (src && /^https?:\/\//i.test(src)) {
        img.setAttribute("data-orig-src", src);
        img.setAttribute("src", proxyImageUrl(src, pageUrl));
        img.setAttribute("loading", "lazy");
      }
    });
    tpl.content.querySelectorAll("a").forEach((a) => {
      a.setAttribute("target", "_blank");
      a.setAttribute("rel", "noopener noreferrer");
    });
    return tpl.innerHTML;
  }

  function renderMarkdown(md) {
    const lines = escapeHtml(md).split(/\r?\n/);
    let html = "";
    let inUl = false;
    for (const raw of lines) {
      const line = raw.trim();
      if (!line) {
        if (inUl) {
          html += "</ul>";
          inUl = false;
        }
        continue;
      }
      const li = line.match(/^[-*•]\s+(.*)$/);
      if (li) {
        if (!inUl) {
          html += "<ul>";
          inUl = true;
        }
        html += `<li>${inlineMd(li[1])}</li>`;
        continue;
      }
      if (inUl) {
        html += "</ul>";
        inUl = false;
      }
      const h = line.match(/^#{1,4}\s+(.*)$/);
      if (h) {
        html += `<p><strong>${inlineMd(h[1])}</strong></p>`;
      } else {
        html += `<p>${inlineMd(line)}</p>`;
      }
    }
    if (inUl) html += "</ul>";
    return html;
  }

  function inlineMd(s) {
    return s
      .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
      .replace(/`([^`]+)`/g, "<code>$1</code>");
  }

  function getActiveIntegrations() {
    const raw = state.integrations;
    if (!raw || !Array.isArray(raw.items)) return { useAiSummary: false, items: [] };
    const names = BUILTIN_NAMES[state.lang] || BUILTIN_NAMES.en;
    const items = raw.items.map((item) => ({
      id: item.id,
      kind: item.kind,
      name: names[item.kind] || item.name || item.kind,
    }));
    return { useAiSummary: raw.useAiSummary === true, items };
  }

  function updateStaticLabels() {
    $("search-input").placeholder = tr("searchPlaceholder");
    $("sheet-title").textContent = tr("shareTitle");
    $("sheet-cancel").textContent = tr("cancel");
    document.querySelectorAll("[data-i18n]").forEach((el) => {
      const key = el.getAttribute("data-i18n");
      if (key) el.textContent = tr(key);
    });
  }

  function currentCategoryLabel() {
    const q = state.query;
    if (q.kind === "unread") return tr("unread");
    if (q.kind === "all") return tr("all");
    if (q.kind === "starred") return tr("starred");
    if (q.kind === "readLater") return tr("readLater");
    if (q.kind === "folder") {
      const f = state.folders.find((x) => x.id === q.value);
      return f ? f.name : tr("folders");
    }
    if (q.kind === "feed") {
      const f = state.feeds.find((x) => x.id === q.value);
      return f ? f.title : tr("feeds");
    }
    return "Papr";
  }

  function renderDrawer() {
    const box = $("drawer-content");
    const q = state.query;
    const isAct = (kind, value = null) =>
      q.kind === kind && (value === null || q.value === value) ? "active" : "";

    let html = "";
    const smart = [
      { kind: "unread", label: tr("unread"), count: state.counts.unread },
      { kind: "all", label: tr("all"), count: "" },
      { kind: "starred", label: tr("starred"), count: state.counts.starred },
      { kind: "readLater", label: tr("readLater"), count: state.counts.readLater },
    ];
    for (const s of smart) {
      html += `<button class="nav-item ${isAct(s.kind)}" data-kind="${s.kind}">
        <span class="nav-item-label">${escapeHtml(s.label)}</span>
        <span class="nav-count">${s.count || ""}</span>
      </button>`;
    }

    if (state.folders.length > 0) {
      html += `<div class="nav-section-title">${escapeHtml(tr("folders"))}</div>`;
      for (const folder of state.folders) {
        const folderFeeds = state.feeds.filter((f) => f.folderId === folder.id);
        const unread = folderFeeds.reduce((sum, f) => sum + (f.unreadCount || 0), 0);
        html += `<button class="nav-item ${isAct("folder", folder.id)}" data-kind="folder" data-value="${folder.id}">
          <span class="nav-item-label">📁 ${escapeHtml(folder.name)}</span>
          <span class="nav-count">${unread || ""}</span>
        </button>`;
        for (const feed of folderFeeds) {
          html += `<button class="nav-item is-child ${isAct("feed", feed.id)}" data-kind="feed" data-value="${feed.id}">
            <span class="nav-item-label">${escapeHtml(feed.title)}</span>
            <span class="nav-count">${feed.unreadCount || ""}</span>
          </button>`;
        }
      }
    }

    const ungrouped = state.feeds.filter((f) => f.folderId == null);
    if (ungrouped.length > 0) {
      html += `<div class="nav-section-title">${escapeHtml(tr("feeds"))}</div>`;
      for (const feed of ungrouped) {
        html += `<button class="nav-item ${isAct("feed", feed.id)}" data-kind="feed" data-value="${feed.id}">
          <span class="nav-item-label">${escapeHtml(feed.title)}</span>
          <span class="nav-count">${feed.unreadCount || ""}</span>
        </button>`;
      }
    }

    box.innerHTML = html;
    box.querySelectorAll(".nav-item").forEach((btn) => {
      btn.addEventListener("click", () => {
        const kind = btn.getAttribute("data-kind");
        const rawVal = btn.getAttribute("data-value");
        state.query = { kind, value: rawVal ? Number(rawVal) : null };
        closeDrawer();
        loadArticles(true);
      });
    });
  }

  function renderArticleList() {
    $("current-category-title").textContent = currentCategoryLabel();
    $("unread-filter-btn").classList.toggle("is-active", state.unreadOnly);
    $("mark-all-btn").disabled = Boolean(state.search.trim());
    $("mark-all-btn").style.opacity = state.search.trim() ? "0.35" : "";

    const listEl = $("article-list");
    if (state.articles.length === 0 && !state.loadingList) {
      listEl.innerHTML = `<div class="empty-state">${escapeHtml(tr("empty"))}</div>`;
      return;
    }

    let html = "";
    for (const a of state.articles) {
      const thumb = a.imageUrl
        ? `<img class="article-thumb" src="${escapeHtml(proxyImageUrl(a.imageUrl, a.url))}" alt="" loading="lazy" />`
        : "";
      html += `<div class="article-card ${a.isRead ? "is-read" : ""}" data-id="${a.id}">
        <div class="article-card-body">
          <div class="article-meta-row">
            <span class="unread-dot"></span>
            <span class="article-feed-label">${escapeHtml(a.feedTitle || "")}</span>
            ${a.isStarred ? '<span class="article-star-badge">★</span>' : ""}
            <span class="article-time">${escapeHtml(relTime(a.publishedAt || a.fetchedAt))}</span>
          </div>
          <h3 class="article-title">${escapeHtml(a.title || "(Untitled)")}</h3>
          ${a.summary ? `<p class="article-excerpt">${escapeHtml(a.summary)}</p>` : ""}
        </div>
        ${thumb}
      </div>`;
    }

    if (state.hasMore) {
      html += `<div class="list-footer">
        <button id="load-more-btn" class="load-more-btn">${escapeHtml(tr("loadMore"))}</button>
      </div>`;
    }

    listEl.innerHTML = html;
    listEl.querySelectorAll(".article-card").forEach((card) => {
      card.addEventListener("click", () => {
        const id = Number(card.getAttribute("data-id"));
        openArticle(id);
      });
    });
    const moreBtn = $("load-more-btn");
    if (moreBtn) {
      moreBtn.addEventListener("click", () => loadArticles(false));
    }
  }

  function renderReader() {
    const a = state.currentArticle;
    if (!a) return;

    $("reader-feed-name").textContent = a.feedTitle || "";
    const readBtn = $("reader-read-btn");
    readBtn.classList.toggle("is-active", !a.isRead);
    readBtn.innerHTML = a.isRead
      ? `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="8" /></svg>`
      : `<svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor"><circle cx="12" cy="12" r="6" /></svg>`;

    const title =
      state.showTranslation && a.translatedTitle ? a.translatedTitle : a.title;
    const rawBody =
      state.showTranslation && a.translatedHtml
        ? a.translatedHtml
        : a.extractedHtml || a.contentHtml || `<p>${escapeHtml(a.summary || "")}</p>`;
    const bodyHtml = rewriteHtmlImages(rawBody, a.url);

    let summaryHtml = "";
    if (a.aiSummary && a.aiSummary.trim()) {
      summaryHtml = `<section class="ai-summary-card">
        <div class="ai-summary-head">
          <span>✨ ${escapeHtml(tr("aiSummary"))}</span>
        </div>
        <div class="ai-summary-body">${renderMarkdown(a.aiSummary)}</div>
      </section>`;
    }

    const bylineParts = [];
    if (a.author) bylineParts.push(escapeHtml(a.author));
    if (a.publishedAt) bylineParts.push(escapeHtml(relTime(a.publishedAt)));
    if (state.showTranslation && a.translatedHtml) {
      bylineParts.push(`<span class="reader-badge">${escapeHtml(tr("translatedBadge"))}</span>`);
    }

    $("reader-article").innerHTML = `
      <h1 class="reader-title">${escapeHtml(title || "(Untitled)")}</h1>
      <div class="reader-byline">${bylineParts.join(" · ")}</div>
      ${summaryHtml}
      <div class="reader-content">${bodyHtml}</div>
    `;

    // Bottom bar states
    const trBtn = $("btn-translate");
    trBtn.classList.toggle("is-active", state.showTranslation);
    trBtn.querySelector("span").textContent = state.translating
      ? tr("translating")
      : state.showTranslation
      ? tr("original")
      : tr("translate");
    trBtn.disabled = state.translating;

    const sumBtn = $("btn-summarize");
    sumBtn.classList.toggle("is-active", Boolean(a.aiSummary));
    sumBtn.querySelector("span").textContent = state.summarizing
      ? tr("summarizing")
      : tr("summarize");
    sumBtn.disabled = state.summarizing;

    const starBtn = $("btn-star");
    starBtn.classList.toggle("is-starred", Boolean(a.isStarred));
    starBtn.querySelector("span").textContent = a.isStarred
      ? tr("starredBtn")
      : tr("star");
  }

  async function loadBootstrap(silent = false) {
    try {
      const data = await apiFetch("/api/bootstrap");
      state.folders = data.folders || [];
      state.feeds = data.feeds || [];
      state.counts = data.counts || { unread: 0, starred: 0, readLater: 0 };
      state.integrations = data.integrations || null;
      state.translateEngine = data.translateEngine || "llm";
      state.translateTargetLang = data.translateTargetLang || "zh";
      if (data.language && I18N[data.language]) {
        state.lang = data.language;
      }
      updateStaticLabels();
      renderDrawer();
      $("current-category-title").textContent = currentCategoryLabel();
    } catch (e) {
      if (!silent) showToast(e.message);
    }
  }

  let listSeq = 0;
  let loadingMore = false;
  const PAGE_LIMIT = 100; // server-side cap for /api/articles

  // Rows the current filter still returns from the server. Opening an article
  // marks it read, which shrinks the server's unread set — so the offset for
  // "Load more" must count matching rows, not the raw loaded length, or the
  // next page skips unread articles.
  function matchesCurrentQuery(a) {
    return !((state.query.kind === "unread" || state.unreadOnly) && a.isRead);
  }

  function articleParams(limit, offset) {
    const params = new URLSearchParams({
      kind: state.query.kind,
      unreadOnly: String(state.unreadOnly),
      limit: String(limit),
      offset: String(offset),
    });
    if (state.query.value != null) params.set("value", String(state.query.value));
    if (state.search.trim()) params.set("search", state.search.trim());
    return params;
  }

  // Fetch `count` rows from `offset` in server-sized pages: one request is
  // clamped to 100, so refreshing a longer loaded list must be batched instead
  // of replacing it with a truncated first page.
  async function fetchArticles(count, offset) {
    const rows = [];
    while (rows.length < count) {
      const limit = Math.min(PAGE_LIMIT, count - rows.length);
      const page = await apiFetch(`/api/articles?${articleParams(limit, offset + rows.length)}`);
      rows.push(...page);
      if (page.length < limit) break;
    }
    return rows;
  }

  async function loadArticles(reset = true, preserveScroll = false) {
    if (!reset && loadingMore) return;
    const seq = reset ? ++listSeq : listSeq;
    if (reset) {
      state.loadingList = true;
    } else {
      loadingMore = true;
    }
    const offset = reset ? 0 : state.articles.filter(matchesCurrentQuery).length;
    const count = reset && preserveScroll ? Math.max(40, state.articles.length) : 40;

    try {
      const rows = await fetchArticles(count, offset);
      if (seq !== listSeq) return;
      if (reset) {
        const prevScroll = $("article-list").scrollTop;
        state.articles = rows;
        $("article-list").scrollTop = preserveScroll ? prevScroll : 0;
      } else {
        const seen = new Set(state.articles.map((a) => a.id));
        state.articles = state.articles.concat(rows.filter((a) => !seen.has(a.id)));
      }
      state.hasMore = rows.length === count;
      renderArticleList();
    } catch (e) {
      if (seq === listSeq) showToast(e.message);
    } finally {
      if (seq === listSeq) state.loadingList = false;
      loadingMore = false;
    }
  }

  let openSeq = 0;
  const readMutSeq = new Map();
  const starMutSeq = new Map();

  async function openArticle(id) {
    const seq = ++openSeq;
    try {
      const detail = await apiFetch(`/api/articles/${id}`);
      if (seq !== openSeq) return;
      state.currentArticle = detail;
      state.showTranslation = Boolean(detail.translatedHtml);
      state.translating = false;
      state.summarizing = false;
      $("reader-scroll").scrollTop = 0;
      $("reader-view").classList.add("active");
      renderReader();

      if (!detail.isRead) {
        const mutSeq = (readMutSeq.get(id) || 0) + 1;
        readMutSeq.set(id, mutSeq);
        patchLocalArticle(id, { isRead: true });
        renderReader();
        apiFetch(`/api/articles/${id}/read`, {
          method: "POST",
          body: JSON.stringify({ read: true }),
        })
          .then(() => loadBootstrap(true))
          .catch((e) => {
            if (readMutSeq.get(id) === mutSeq) {
              patchLocalArticle(id, { isRead: false });
              if (state.currentArticle && state.currentArticle.id === id) renderReader();
              showToast(e.message);
            }
          });
      }
    } catch (e) {
      if (seq === openSeq) showToast(e.message);
    }
  }

  function patchLocalArticle(id, patch) {
    state.articles = state.articles.map((x) => (x.id === id ? { ...x, ...patch } : x));
    if (state.currentArticle && state.currentArticle.id === id) {
      state.currentArticle = { ...state.currentArticle, ...patch };
    }
    renderArticleList();
  }

  function closeReader() {
    $("reader-view").classList.remove("active");
    state.currentArticle = null;
  }

  function openDrawer() {
    $("drawer-backdrop").classList.remove("hidden");
    $("drawer").classList.add("open");
    $("drawer").setAttribute("aria-hidden", "false");
  }

  function closeDrawer() {
    $("drawer-backdrop").classList.add("hidden");
    $("drawer").classList.remove("open");
    $("drawer").setAttribute("aria-hidden", "true");
  }

  async function copyArticleLink() {
    const url = state.currentArticle && state.currentArticle.url;
    if (!url) {
      showToast(tr("noUrl"));
      return;
    }
    try {
      if (navigator.clipboard && navigator.clipboard.writeText) {
        await navigator.clipboard.writeText(url);
      } else {
        const ta = document.createElement("textarea");
        ta.value = url;
        ta.style.position = "fixed";
        ta.style.top = "0";
        ta.style.left = "-9999px";
        document.body.appendChild(ta);
        ta.focus();
        ta.select();
        ta.setSelectionRange(0, url.length);
        const ok = document.execCommand("copy");
        document.body.removeChild(ta);
        if (!ok) throw new Error("execCommand failed");
      }
      showToast(tr("copied"));
    } catch {
      showToast(tr("copyFail"));
    }
  }

  function openShareSheet() {
    const a = state.currentArticle;
    if (!a) return;
    const { items } = getActiveIntegrations();
    const list = $("sheet-list");
    if (items.length === 0) {
      list.innerHTML = `<div class="empty-state" style="padding:20px 10px">${escapeHtml(tr("noIntegrations"))}</div>`;
    } else {
      list.innerHTML = items
        .map(
          (item) => `<button class="sheet-item" data-id="${escapeHtml(item.id)}">
            <span>${escapeHtml(item.name)}</span>
            <span style="color:var(--text-faint)">→</span>
          </button>`
        )
        .join("");
      list.querySelectorAll(".sheet-item").forEach((btn) => {
        btn.addEventListener("click", async () => {
          const item = items.find((x) => x.id === btn.getAttribute("data-id"));
          if (!item) return;
          closeShareSheet();
          try {
            showToast(`${item.name}…`);
            const res = await apiFetch(`/api/articles/${a.id}/integration`, {
              method: "POST",
              body: JSON.stringify({ id: item.id }),
            });
            if (res && res.clientOpen && res.url) {
              showToast(tr("sentTo", { name: item.name }), res.url, tr("openApp"));
              window.location.href = res.url;
            } else {
              showToast(tr("sentTo", { name: item.name }));
            }
          } catch (e) {
            showToast(e.message);
          }
        });
      });
    }
    $("sheet-backdrop").classList.remove("hidden");
    $("integration-sheet").classList.remove("hidden");
  }

  function closeShareSheet() {
    $("sheet-backdrop").classList.add("hidden");
    $("integration-sheet").classList.add("hidden");
  }

  async function syncNow(silent = false, preserveScroll = false) {
    const btn = $("sync-btn");
    btn.classList.add("spinning");
    try {
      await loadBootstrap(silent);
      await loadArticles(true, preserveScroll);
      if (!silent) showToast(tr("synced"));
    } finally {
      btn.classList.remove("spinning");
    }
  }

  function bindEvents() {
    $("menu-btn").addEventListener("click", openDrawer);
    $("title-cat-btn").addEventListener("click", openDrawer);
    $("drawer-close").addEventListener("click", closeDrawer);
    $("drawer-backdrop").addEventListener("click", closeDrawer);

    $("unread-filter-btn").addEventListener("click", () => {
      state.unreadOnly = !state.unreadOnly;
      loadArticles(true);
    });

    $("mark-all-btn").addEventListener("click", async () => {
      if (state.search.trim()) return;
      if (!window.confirm(tr("markAllConfirm"))) return;
      try {
        await apiFetch("/api/articles/mark-all-read", {
          method: "POST",
          body: JSON.stringify({
            kind: state.query.kind,
            value: state.query.value,
          }),
        });
        showToast(tr("markedAllRead"));
        await syncNow(true);
      } catch (e) {
        showToast(e.message);
      }
    });

    $("sync-btn").addEventListener("click", () => syncNow(false));

    let searchTimer = null;
    $("search-input").addEventListener("input", (e) => {
      state.search = e.target.value;
      clearTimeout(searchTimer);
      searchTimer = setTimeout(() => loadArticles(true), 250);
    });

    $("back-btn").addEventListener("click", closeReader);

    $("reader-read-btn").addEventListener("click", async () => {
      const a = state.currentArticle;
      if (!a) return;
      const targetId = a.id;
      const prev = Boolean(a.isRead);
      const next = !prev;
      const mutSeq = (readMutSeq.get(targetId) || 0) + 1;
      readMutSeq.set(targetId, mutSeq);
      patchLocalArticle(targetId, { isRead: next });
      renderReader();
      try {
        await apiFetch(`/api/articles/${targetId}/read`, {
          method: "POST",
          body: JSON.stringify({ read: next }),
        });
        loadBootstrap(true);
      } catch (e) {
        if (readMutSeq.get(targetId) === mutSeq) {
          patchLocalArticle(targetId, { isRead: prev });
          if (state.currentArticle && state.currentArticle.id === targetId) renderReader();
        }
        showToast(e.message);
      }
    });

    $("reader-fulltext-btn").addEventListener("click", async () => {
      const a = state.currentArticle;
      if (!a) return;
      const targetId = a.id;
      const btn = $("reader-fulltext-btn");
      btn.classList.add("spinning");
      try {
        const res = await apiFetch(`/api/articles/${targetId}/fulltext`, { method: "POST" });
        if (!state.currentArticle || state.currentArticle.id !== targetId) return;
        state.currentArticle.extractedHtml = res.html;
        state.showTranslation = false;
        renderReader();
        showToast(tr("fulltextDone"));
      } catch (e) {
        showToast(e.message);
      } finally {
        btn.classList.remove("spinning");
      }
    });

    $("btn-translate").addEventListener("click", async () => {
      const a = state.currentArticle;
      if (!a || state.translating) return;
      const targetId = a.id;
      if (a.translatedHtml) {
        state.showTranslation = !state.showTranslation;
        renderReader();
        return;
      }
      state.translating = true;
      renderReader();
      try {
        const res = await apiFetch(`/api/articles/${targetId}/translate`, {
          method: "POST",
          body: JSON.stringify({
            lang: state.translateTargetLang,
            engine: state.translateEngine,
          }),
        });
        if (!state.currentArticle || state.currentArticle.id !== targetId) return;
        state.currentArticle.translatedHtml = res.translatedHtml;
        state.currentArticle.translatedTitle = res.translatedTitle;
        state.currentArticle.translatedLang = res.translatedLang;
        state.showTranslation = true;
      } catch (e) {
        if (state.currentArticle && state.currentArticle.id === targetId) showToast(e.message);
      } finally {
        if (state.currentArticle && state.currentArticle.id === targetId) {
          state.translating = false;
          renderReader();
        }
      }
    });

    $("btn-summarize").addEventListener("click", async () => {
      const a = state.currentArticle;
      if (!a || state.summarizing) return;
      const targetId = a.id;
      if (a.aiSummary && a.aiSummary.trim()) {
        $("reader-scroll").scrollTo({ top: 0, behavior: "smooth" });
        return;
      }
      state.summarizing = true;
      renderReader();
      try {
        const res = await apiFetch(`/api/articles/${targetId}/summarize`, { method: "POST" });
        if (!state.currentArticle || state.currentArticle.id !== targetId) return;
        state.currentArticle.aiSummary = res.summary;
        $("reader-scroll").scrollTo({ top: 0, behavior: "smooth" });
      } catch (e) {
        if (state.currentArticle && state.currentArticle.id === targetId) showToast(e.message);
      } finally {
        if (state.currentArticle && state.currentArticle.id === targetId) {
          state.summarizing = false;
          renderReader();
        }
      }
    });

    $("btn-star").addEventListener("click", async () => {
      const a = state.currentArticle;
      if (!a) return;
      const targetId = a.id;
      const prev = Boolean(a.isStarred);
      const next = !prev;
      const mutSeq = (starMutSeq.get(targetId) || 0) + 1;
      starMutSeq.set(targetId, mutSeq);
      patchLocalArticle(targetId, { isStarred: next });
      renderReader();
      try {
        await apiFetch(`/api/articles/${targetId}/starred`, {
          method: "POST",
          body: JSON.stringify({ starred: next }),
        });
        loadBootstrap(true);
      } catch (e) {
        if (starMutSeq.get(targetId) === mutSeq) {
          patchLocalArticle(targetId, { isStarred: prev });
          if (state.currentArticle && state.currentArticle.id === targetId) renderReader();
        }
        showToast(e.message);
      }
    });

    $("btn-copy").addEventListener("click", copyArticleLink);

    $("btn-browser").addEventListener("click", () => {
      const url = state.currentArticle && state.currentArticle.url;
      if (!url || !/^https?:\/\//i.test(url)) {
        showToast(tr("noUrl"));
        return;
      }
      window.open(url, "_blank", "noopener,noreferrer");
    });

    $("btn-share").addEventListener("click", openShareSheet);
    $("sheet-backdrop").addEventListener("click", closeShareSheet);
    $("sheet-cancel").addEventListener("click", closeShareSheet);

    // Sync with PC when returning to app/tab or periodically while visible.
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") {
        syncNow(true, true);
      }
    });
    setInterval(() => {
      if (document.visibilityState === "visible" && !state.currentArticle) {
        syncNow(true, true);
      }
    }, 15000);
  }

  async function init() {
    const urlParams = new URLSearchParams(window.location.search);
    const pairCode = urlParams.get("pair");
    if (pairCode && pairCode.trim()) {
      try {
        const res = await fetch("/api/pair", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ code: pairCode.trim() }),
        });
        if (res.ok) {
          const data = await res.json();
          if (data && data.token) setToken(data.token);
        }
      } catch {}
      window.history.replaceState(null, "", window.location.pathname);
    } else if (getToken()) {
      // Refresh HttpOnly cookie for <img> requests on session restore
      await fetch("/api/auth", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${getToken()}`,
        },
        body: "{}",
      }).catch(() => {});
    }
    // No service worker: the LAN server is plain HTTP, and browsers only
    // install SWs on secure origins. "Add to Home Screen" still gives the
    // standalone UI from the manifest.
    bindEvents();
    await loadBootstrap(false);
    await loadArticles(true);
  }

  init();
})();
