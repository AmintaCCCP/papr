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
pub fn template_matches(concrete: &str, template: &str) -> bool {
    let concrete = concrete.trim_matches('/');
    let template = template.trim_matches('/');
    let mut t_segments = template.split('/').peekable();
    for c in concrete.split('/') {
        let Some(t) = t_segments.next() else {
            // The concrete path is deeper than the template — no match.
            return false;
        };
        let is_param = t.starts_with(':');
        let is_optional = t.ends_with('?');
        if is_param {
            // `:seg?` may be absent, which shows up as an empty segment only
            // when the caller kept the slash. Empty concrete segments match
            // optional params; a required param must have content.
            if c.is_empty() && !is_optional {
                return false;
            }
        } else if c != t {
            return false;
        }
    }
    // Anything the concrete path did not consume must be optional
    // (`:tail?`) — a required segment left unfilled is not a match.
    t_segments.all(|t| t.ends_with('?'))
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
}
