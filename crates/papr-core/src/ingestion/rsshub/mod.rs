//! The RSSHub route marketplace: a bundled route catalog, a registry of
//! installed routes, and a local engine that executes routes on the user's
//! machine.
//!
//! RSSHub documents thousands of site-specific routes; the marketplace lets
//! the user browse the bundled catalog, install the ones they want (with any
//! environment variables the route requires) and subscribe to them with
//! `rsshub://<route path>` feed URLs. Subscriptions are ordinary `feeds` rows
//! whose `source_type` is `'rsshub'`; refreshing runs the route through the
//! bundled local engine (see [`engine`]) — papr never talks to a hosted
//! RSSHub instance.

pub mod catalog;
pub mod engine;

/// Extract the route path from an `rsshub://` feed URL, lowercase-insensitive
/// about the scheme and tolerant of extra leading slashes.
///
/// `rsshub:///bilibili/user/video/2267573` and `RSSHub://bilibili/user/video/2267573`
/// both yield `bilibili/user/video/2267573`. Returns `None` for anything that
/// is not an `rsshub://` link or whose route part is empty. The route keeps
/// the case and shape the user typed — RSSHub paths are case-sensitive.
pub fn route_from_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    // "rsshub://" is pure ASCII, so byte-slicing past it is safe.
    if !trimmed
        .get(..9)
        .is_some_and(|s| s.eq_ignore_ascii_case("rsshub://"))
    {
        return None;
    }
    let route = trimmed[9..].trim_matches('/');
    if route.is_empty() {
        return None;
    }
    Some(route.to_string())
}

/// Environment-variable entry limits, shared by the marketplace's save path
/// (the Tauri command validates against them) and the engine's env merge
/// (`rsshub_enabled_env` filters stale or hand-edited rows against them, so
/// one bad row cannot wedge the spawn with an unexplainable OS error).
/// The value ceiling keeps a single entry clear of the `execve` E2BIG
/// threshold (~128 KiB on 4 KiB-page Linux systems) with room to spare.
pub const MAX_ENV_KEY_BYTES: usize = 128;
pub const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;

/// `true` for environment variable names a child process can carry:
/// `[A-Za-z_][A-Za-z0-9_]*` — the shape of every variable RSSHub documents
/// (`BILIBILI_COOKIE_<uid>`, `GITHUB_TOKEN`, …). Anything else (`=`, spaces,
/// control characters, the empty name) is refused.
pub fn valid_env_key(key: &str) -> bool {
    if key.len() > MAX_ENV_KEY_BYTES {
        return false;
    }
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `true` for environment values an `execve` can carry: no NUL (it
/// terminates the C string) and within the length ceiling.
pub fn valid_env_value(value: &str) -> bool {
    !value.contains('\0') && value.len() <= MAX_ENV_VALUE_BYTES
}

/// Split a route template into segments, treating a `/` inside a `{…}`
/// regex constraint as part of the segment rather than a separator: hono
/// applies the brace regex to the remaining path, and the npm scoped-package
/// template (`:name{(@[a-z0-9-~][a-z0-9-._~]*/)?…}`) carries a literal `/`
/// inside the braces that must not split it into a param fragment plus a
/// garbage literal.
fn split_template_segments(template: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, ch) in template.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '/' if depth == 0 => {
                segments.push(&template[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    segments.push(&template[start..]);
    segments
}

/// True when a concrete route invocation (`concrete`, e.g. a feed's
/// `rsshub://<path>` path part) instantiates the installed route `template`.
///
/// `route_path` in the registry stores the *template* the catalog documents —
/// `/bilibili/user/video/:id` — while a subscription pins concrete values:
/// `/bilibili/user/video/2267573`. A template segment starting with `:` (with
/// an optional trailing `?` marking it optional) matches any non-empty,
/// slash-free segment; every other segment must match exactly. A concrete
/// path may be shorter than its template only if the trailing template
/// segments are all optional.
///
/// Two shapes need extra care:
///
/// * A query string (`rsshub:///bilibili/hot/search?keyword=x`) travels with
///   the route into the engine URL, but matching is path-shaped — the query
///   never participates.
/// * A regex-constrained parameter (`:name{.+}`, hono's syntax — 174 catalog
///   routes use it, `/rss/:url{.+}` foremost) applies its regex to the *rest
///   of the path*, so it consumes every remaining segment instead of one.
///   The `{.+}` shape can never match a single segment, so without this the
///   affected subscriptions would silently never refresh.
pub fn template_matches(concrete: &str, template: &str) -> bool {
    let path = concrete.split(['?', '#']).next().unwrap_or("");
    let c_segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let t_segments: Vec<&str> = split_template_segments(template.trim_matches('/'));
    let mut consumed = 0usize;
    for (index, t) in t_segments.iter().enumerate() {
        let is_param = t.starts_with(':');
        let is_optional = t.ends_with('?');
        // A regex-constrained param spans slashes (hono applies the regex to
        // the remaining path), so it eats the rest — template segments after
        // it could never match anyway. The concrete path must have something
        // left for it unless it is optional.
        if is_param && t.contains('{') {
            return if consumed < c_segments.len() {
                true
            } else {
                is_optional
            };
        }
        match c_segments.get(consumed) {
            Some(&c) => {
                if is_param {
                    // `:seg?` may be absent, which shows up as an empty
                    // segment only when the caller kept the slash. Empty
                    // concrete segments match optional params; a required
                    // param must have content.
                    if c.is_empty() && !is_optional {
                        return false;
                    }
                } else if c != *t {
                    return false;
                }
                consumed += 1;
            }
            None => {
                // The concrete path ended mid-template: every remaining
                // template segment must be optional.
                return t_segments[index..].iter().all(|s| s.ends_with('?'));
            }
        }
    }
    // Anything the concrete path did not consume must be optional
    // (`:tail?`) — a required segment left unfilled is not a match.
    consumed == c_segments.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_from_url_strips_scheme_and_slashes() {
        assert_eq!(
            route_from_url("rsshub:///bilibili/user/video/2267573").as_deref(),
            Some("bilibili/user/video/2267573")
        );
        assert_eq!(
            route_from_url("rsshub://github/issue/DIYgod/RSSHub").as_deref(),
            Some("github/issue/DIYgod/RSSHub")
        );
    }

    #[test]
    fn route_from_url_keeps_case_and_query() {
        // RSSHub paths are case-sensitive and may carry query strings.
        assert_eq!(
            route_from_url("  RSSHub://Bilibili/Hot/Search?keyword=x  ").as_deref(),
            Some("Bilibili/Hot/Search?keyword=x")
        );
    }

    #[test]
    fn route_from_url_rejects_non_rsshub_and_empty() {
        assert_eq!(route_from_url("https://rsshub.app/foo"), None);
        assert_eq!(route_from_url("rsshub://"), None);
        assert_eq!(route_from_url("rsshub:///"), None);
        assert_eq!(route_from_url("rss"), None);
        assert_eq!(route_from_url(""), None);
    }

    #[test]
    fn template_matches_concrete_segments() {
        assert!(template_matches(
            "/bilibili/user/video/2267573",
            "/bilibili/user/video/:id"
        ));
        assert!(template_matches(
            "bilibili/user/video/2267573",
            "/bilibili/user/video/:id/"
        ));
        assert!(template_matches(
            "/github/issue/DIYgod/RSSHub",
            "/github/issue/:user/:repo"
        ));
    }

    #[test]
    fn template_matches_literal_segments_exactly() {
        assert!(!template_matches(
            "/bilibili/user/board/2267573",
            "/bilibili/user/video/:id"
        ));
        // A literal segment must not be matched by a different literal.
        assert!(!template_matches("/github/issue/x/y", "/github/pull/x/y"));
        // Route prefixes must not shadow each other.
        assert!(!template_matches(
            "/bilibili/user/video/1/extra",
            "/bilibili/user/video/:id"
        ));
    }

    #[test]
    fn template_matches_optional_params() {
        // `:seg?` may be omitted (an empty segment or simply absent).
        assert!(template_matches(
            "/bilibili/platform/-1",
            "/bilibili/platform/:area?"
        ));
        assert!(template_matches(
            "/bilibili/platform/-1/all",
            "/bilibili/platform/:area?/:type?"
        ));
        // …but a required `:seg` with no value is not a match.
        assert!(!template_matches(
            "/bilibili/user/video/",
            "/bilibili/user/video/:id"
        ));
        assert!(!template_matches(
            "/bilibili/user/video",
            "/bilibili/user/video/:id"
        ));
    }

    #[test]
    fn template_matches_exact_route_without_params() {
        assert!(template_matches(
            "/github/activity/DIYgod",
            "/github/activity/:user"
        ));
        assert!(template_matches("/slack/user/:id", "/slack/user/:id"));
        assert!(!template_matches("/slack/user/", "/slack/user/:id"));
    }

    #[test]
    fn template_matches_ignores_query_and_fragment() {
        // A query travels with the route into the engine URL but never
        // participates in matching.
        assert!(template_matches(
            "/bilibili/hot/search?keyword=x",
            "/bilibili/hot/search"
        ));
        assert!(template_matches(
            "github/activity/DIYgod#top",
            "/github/activity/:user"
        ));
        assert!(!template_matches(
            "/bilibili/hot/other?keyword=x",
            "/bilibili/hot/search"
        ));
    }

    #[test]
    fn template_matches_catch_all_regex_params() {
        // hono's `:name{.+}` applies the regex to the rest of the path —
        // it must consume every remaining segment.
        assert!(template_matches(
            "/81/81rc/sy/gzdt_210283",
            "/81/81rc/:category{.+}?"
        ));
        assert!(template_matches(
            "/rss/https://example.com/blog/feed.xml",
            "/rss/:url{.+}"
        ));
        // A deep concrete path under a catch-all template still matches.
        assert!(template_matches(
            "/1x/category/sub/deep",
            "/1x/:category{.+}?"
        ));
        // An optional catch-all with nothing left matches; a required one
        // does not.
        assert!(template_matches("/81/81rc", "/81/81rc/:category{.+}?"));
        assert!(!template_matches("/rss", "/rss/:url{.+}"));
        // Literals before the catch-all still must match exactly.
        assert!(!template_matches(
            "/81/other/sy/gzdt_210283",
            "/81/81rc/:category{.+}?"
        ));
        // A concrete path ending where the catch-all sits is consumed by it,
        // not left dangling.
        assert!(template_matches(
            "/bangumi.moe/tag1/tag2",
            "/bangumi.moe/:tags{.+}?"
        ));
    }

    #[test]
    fn template_matches_does_not_split_inside_braces() {
        // The npm scoped-package template carries a literal `/` inside its
        // regex: splitting there would turn the tail into a garbage literal
        // segment no concrete path could satisfy.
        let npm = "/npm/package/:name{(@[a-z0-9-~][a-z0-9-._~]*/)?[a-z0-9-~][a-z0-9-._~]*}";
        assert!(template_matches("/npm/package/react", npm));
        assert!(template_matches("/npm/package/@rsshub/app", npm));
        assert!(!template_matches("/npm/package", npm));
        assert!(!template_matches("/npm/other/react", npm));
    }

    #[test]
    fn valid_env_keys_and_values_reject_what_exec_cannot_carry() {
        assert!(valid_env_key("GITHUB_TOKEN"));
        assert!(valid_env_key("_X"));
        assert!(valid_env_key("BILIBILI_COOKIE_123"));
        assert!(!valid_env_key(""));
        assert!(!valid_env_key("9BAD"));
        assert!(!valid_env_key("A=B"));
        assert!(!valid_env_key("A B"));
        assert!(!valid_env_key(&"x".repeat(MAX_ENV_KEY_BYTES + 1)));
        assert!(valid_env_value("token-value with spaces"));
        assert!(!valid_env_value("a\0b"));
        assert!(!valid_env_value(&"x".repeat(MAX_ENV_VALUE_BYTES + 1)));
    }
}
