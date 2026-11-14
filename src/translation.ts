// Background article-translation jobs. A translation runs to completion
// independently of which article is on screen, so several can be in flight at
// once and switching articles never interrupts them. The reader subscribes to
// the job for the article it is showing; progress arrives per batch (not per
// token) so the webview's main thread stays responsive on a long article.

import { create } from "zustand";
import * as api from "./api";
import { reportError } from "./toast";

export type JobStatus = "translating" | "done" | "error";

export interface TranslationJob {
  status: JobStatus;
  /** Batches completed so far. */
  done: number;
  /** Total batches the body was split into (0 until the first event arrives). */
  total: number;
  /** Accumulated translated HTML, grown as batches arrive. */
  html: string;
  /** Translated article title, set once before the body batches. */
  title: string;
  /** The target language code this job was started for. */
  lang: string;
  /** The engine this job was started with (`llm` / `google` / `deepl` / `bing`). */
  engine: string;
  /** Identity of this job run. Starting a new translation replaces the store
   *  entry, but the replaced job's IPC channel keeps delivering events — every
   *  handler checks this token against the store before patching, so a late
   *  event from a dead run can never write into its replacement. */
  token: number;
}

interface TranslationState {
  jobs: Record<number, TranslationJob>;
  /** Start a background translation for an article into `lang` with `engine`. A
   *  no-op if an identical job (same language and engine) is already running; any
   *  other in-flight or finished job is replaced (e.g. to retry, switch language,
   *  or switch engine). */
  translate: (articleId: number, lang: string, engine: string) => void;
  /** Drop a job (e.g. after its cached result has been read back from the DB). */
  clear: (articleId: number) => void;
}

let nextToken = 1;

export const useTranslationJobs = create<TranslationState>((set, get) => {
  // Patch the article's job only when `token` is still the registered run —
  // see `token` on TranslationJob.
  const patchIfCurrent = (
    articleId: number,
    token: number,
    fn: (j: TranslationJob) => TranslationJob,
  ) =>
    set((s) => {
      const cur = s.jobs[articleId];
      if (!cur || cur.token !== token) return s;
      return { jobs: { ...s.jobs, [articleId]: fn(cur) } };
    });

  return {
    jobs: {},

    translate: (articleId, lang, engine) => {
      const cur = get().jobs[articleId];
      // An identical job already streaming — leave it alone. A job for a
      // different language/engine (or a finished one) is replaced below so the
      // new choice takes effect.
      if (
        cur?.status === "translating" &&
        cur.lang === lang &&
        cur.engine === engine
      )
        return;
      const token = nextToken++;
      set((s) => ({
        jobs: {
          ...s.jobs,
          [articleId]: {
            status: "translating",
            done: 0,
            total: 0,
            html: "",
            title: "",
            lang,
            engine,
            token,
          },
        },
      }));

      api
        .aiTranslate(articleId, lang, engine, (e) => {
          if (e.type === "start") {
            patchIfCurrent(articleId, token, (j) => ({ ...j, total: e.data.total }));
          } else if (e.type === "title") {
            patchIfCurrent(articleId, token, (j) => ({ ...j, title: e.data.title }));
          } else if (e.type === "batch") {
            patchIfCurrent(articleId, token, (j) => ({
              ...j,
              done: e.data.done,
              html: j.html + e.data.html,
            }));
          } else if (e.type === "done") {
            patchIfCurrent(articleId, token, (j) => ({ ...j, html: e.data.html, status: "done" }));
          }
        })
        .then(() => {
          // The `done` event normally flips the status; guard against it being
          // missed so a finished job never sticks on "translating".
          patchIfCurrent(articleId, token, (j) =>
            j.status === "translating" ? { ...j, status: "done" } : j,
          );
        })
        .catch((err) => {
          patchIfCurrent(articleId, token, (j) => ({ ...j, status: "error" }));
          reportError(err);
        });
    },

    clear: (articleId) =>
      set((s) => {
        if (!(articleId in s.jobs)) return s;
        const rest = { ...s.jobs };
        delete rest[articleId];
        return { jobs: rest };
      }),
  };
});
