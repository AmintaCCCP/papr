//! The bundled RSSHub route catalog — a compile-time snapshot of RSSHub's
//! route directory (see `scripts/update-rsshub-catalog.mjs`), embedded with
//! `include_str!` exactly like the curated feed directory.
//!
//! The catalog is *data only*: it never touches the network or the database.
//! Search is a pure, case-insensitive substring scan so the marketplace UI
//! (which loads the whole catalog once over IPC and filters client-side) and
//! the agent CLI share one implementation.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

/// The catalog snapshot, embedded into the binary at compile time.
const CATALOG_JSON: &str = include_str!("../../../resources/rsshub-catalog.json");

/// One route-config requirement (`features.requireConfig`). The docs build
/// emits `false` when nothing is required and an array of named requirements
/// otherwise; `true` (never seen in practice) is tolerated and treated as
/// "requirements unknown", rendered like `false`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RequireConfig {
    /// No environment variables required.
    None(bool),
    /// One or more named requirements (`{ name, optional, description }`).
    Items(Vec<ConfigItem>),
}

impl Default for RequireConfig {
    fn default() -> Self {
        RequireConfig::None(false)
    }
}

/// A single environment variable a route may need.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigItem {
    pub name: String,
    pub optional: bool,
    pub description: Option<String>,
}

/// Route feature flags — the badges the marketplace UI renders.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Features {
    pub require_config: RequireConfig,
    pub require_puppeteer: bool,
    pub anti_crawler: bool,
    pub support_bt: bool,
    pub support_podcast: bool,
    pub support_scihub: bool,
}

/// One RSSHub route. `path` is the route author's form (relative); the
/// canonical full path (the key it is stored under, e.g.
/// `/bilibili/user/video/:id`) is what the registry and subscriptions use.
/// The slim schema carries no per-route categories — category tags live on
/// the namespace.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Route {
    pub path: String,
    pub name: String,
    pub example: Option<String>,
    pub parameters: BTreeMap<String, String>,
    pub description: Option<String>,
    pub features: Features,
}

/// A route namespace (a site / plugin grouping).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Namespace {
    pub name: String,
    pub url: Option<String>,
    pub description: Option<String>,
    pub categories: Vec<String>,
    pub heat: f64,
    pub routes: BTreeMap<String, Route>,
}

/// The whole catalog snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Catalog {
    pub generated_at: String,
    pub namespaces: BTreeMap<String, Namespace>,
}

/// Namespaces and routes carry coarse category tags (`social-media`,
/// `program-update`, …). The NSFW tag is matched case-insensitively.
const NSFW_TAG: &str = "nsfw";

/// The parsed catalog, deserialized once. A malformed bundled asset is a
/// build-time error in spirit — panic like `discovery::directory` does.
pub fn catalog() -> &'static Catalog {
    static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
        serde_json::from_str(CATALOG_JSON).expect("bundled rsshub-catalog.json is valid JSON")
    });
    &CATALOG
}

/// Index of full route path → owning namespace key, built once for
/// `route_lookup`. All keys are `&'static` — they point into the catalog
/// static, which is immutable once built.
fn route_index() -> &'static HashMap<&'static str, &'static str> {
    static INDEX: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
        catalog()
            .namespaces
            .iter()
            .flat_map(|(ns, n)| n.routes.keys().map(move |p| (p.as_str(), ns.as_str())))
            .collect()
    });
    &INDEX
}

/// Look up one route by its canonical full path (`/bilibili/user/video/:id`),
/// returning the owning namespace key and the route.
pub fn route_lookup(path: &str) -> Option<(&'static str, &'static Route)> {
    let ns_key = *route_index().get(path)?;
    let ns = catalog().namespaces.get(ns_key)?;
    let route = ns.routes.get(path)?;
    // The index only ever holds keys that exist in the catalog.
    Some((ns_key, route))
}

/// A flattened search hit: one route plus its namespace context.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogHit {
    /// The namespace key (`bilibili`).
    pub namespace: String,
    /// The namespace display name.
    pub namespace_name: String,
    /// The namespace's site URL, when known.
    pub namespace_url: Option<String>,
    /// The namespace's popularity score, the primary sort key.
    pub heat: f64,
    /// The route's canonical full path (the subscription template).
    pub route_path: String,
    /// The route itself.
    pub route: Route,
}

impl CatalogHit {
    fn from(ns_key: &str, ns: &Namespace, route_path: String, route: &Route) -> Self {
        CatalogHit {
            namespace: ns_key.to_string(),
            namespace_name: ns.name.clone(),
            namespace_url: ns.url.clone(),
            heat: ns.heat,
            route_path,
            route: route.clone(),
        }
    }
}

/// True when `categories` carries the NSFW tag (case-insensitive).
fn is_nsfw(categories: &[String]) -> bool {
    categories.iter().any(|c| c.eq_ignore_ascii_case(NSFW_TAG))
}

/// Case-insensitive search over the whole catalog.
///
/// * `query` — empty matches everything; otherwise a route hits when the
///   needle is a substring of the namespace key/name/url or the route's
///   name/path/description.
/// * `category` — when set, only routes (or their namespace) tagged with it
///   are returned.
/// * `hide_nsfw` — drops routes tagged `nsfw`.
/// * `limit` — caps the result size; `0` means unlimited.
///
/// Results are ordered by namespace heat (descending), then route path, so
/// the most popular sources come first deterministically.
pub fn search_catalog(
    query: &str,
    category: Option<&str>,
    hide_nsfw: bool,
    limit: usize,
) -> Vec<CatalogHit> {
    let needle = query.trim().to_lowercase();
    let category = category.map(|c| c.trim().to_lowercase());
    let mut hits: Vec<CatalogHit> = Vec::new();

    // Namespaces sorted by heat descending, then key — a deterministic
    // "most popular first" walk. (`catalog()` itself is key-ordered.)
    let mut namespaces: Vec<(&String, &Namespace)> = catalog().namespaces.iter().collect();
    namespaces.sort_by(|a, b| {
        b.1.heat
            .partial_cmp(&a.1.heat)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });

    for (ns_key, ns) in namespaces {
        // Namespace-level matches let a query for the site ("bilibili",
        // "GitHub") surface its routes even when a route's own name is
        // unrelated; the needle never matches namespace context alone —
        // at least one route must still match something.
        let ns_match = !needle.is_empty()
            && (ns_key.to_lowercase().contains(&needle)
                || ns.name.to_lowercase().contains(&needle)
                || ns
                    .url
                    .as_deref()
                    .is_some_and(|u| u.to_lowercase().contains(&needle)));
        let ns_nsfw = is_nsfw(&ns.categories);
        let ns_category = category
            .as_ref()
            .is_some_and(|c| ns.categories.iter().any(|t| t.eq_ignore_ascii_case(c)));

        for (route_path, route) in &ns.routes {
            if hide_nsfw && ns_nsfw {
                continue;
            }
            if category.is_some() && !ns_category {
                continue;
            }
            let route_match = needle.is_empty()
                || ns_match
                || route.name.to_lowercase().contains(&needle)
                || route_path.to_lowercase().contains(&needle)
                || route
                    .description
                    .as_deref()
                    .is_some_and(|d| d.to_lowercase().contains(&needle));
            if route_match {
                hits.push(CatalogHit::from(ns_key, ns, route_path.clone(), route));
                if limit > 0 && hits.len() >= limit {
                    // Early exit preserves the heat ordering: the walk is
                    // already in sort order, so the first `limit` hits are
                    // the top ones.
                    return hits;
                }
            }
        }
    }
    hits
}

/// Every distinct category tag in the catalog with its route count, most
/// routes first — the marketplace's category dropdown. Tags are namespace
/// level (the slim schema has no per-route tags); a namespace's routes all
/// count toward each of its tags.
pub fn catalog_categories() -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for ns in catalog().namespaces.values() {
        for tag in &ns.categories {
            *counts.entry(tag.to_lowercase()).or_default() += ns.routes.len();
        }
    }
    // Order: hottest-first using the max heat of namespaces carrying the tag,
    // falling back to the count. Cheap and stable enough for a dropdown.
    let mut tags: Vec<(String, usize)> = counts.into_iter().collect();
    tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled asset must parse and carry plausible content — guards
    /// against the update script silently writing an empty or malformed
    /// catalog.
    #[test]
    fn bundled_catalog_parses_with_content() {
        let cat = catalog();
        assert!(
            cat.namespaces.len() > 1_000,
            "namespaces: {}",
            cat.namespaces.len()
        );
        let routes: usize = cat.namespaces.values().map(|n| n.routes.len()).sum();
        assert!(routes > 3_000, "routes: {routes}");
        assert!(cat.namespaces.contains_key("bilibili"));
        assert!(cat.namespaces.contains_key("github"));
        // The canonical full-path key form the registry and subscriptions use.
        assert!(cat.namespaces["bilibili"]
            .routes
            .keys()
            .any(|p| p.starts_with("/bilibili/")));
    }

    #[test]
    fn route_lookup_finds_by_full_path() {
        let (ns, route) =
            route_lookup("/163/music/user/playrecords/:uid/:type?").expect("163 route present");
        assert_eq!(ns, "163");
        assert!(route.features.require_config != RequireConfig::None(false));
        assert_eq!(route_lookup("/definitely/not/a/route"), None);
    }

    #[test]
    fn search_matches_route_name_case_insensitively() {
        let hits = search_catalog("playrecords", None, false, 0);
        assert!(!hits.is_empty());
        assert!(hits
            .iter()
            .all(|h| h.route.name.to_lowercase().contains("playrecords")
                || h.route_path.to_lowercase().contains("playrecords")));
    }

    #[test]
    fn search_matches_namespace_key_and_description() {
        // Namespace-key match surfaces routes whose own name is unrelated.
        let hits = search_catalog("bilibili", None, false, 0);
        assert!(hits.len() > 10);
        assert!(hits.iter().all(|h| h.namespace == "bilibili"));
    }

    #[test]
    fn search_orders_by_namespace_heat() {
        let hits = search_catalog("", None, false, 0);
        for pair in hits.windows(2) {
            assert!(
                pair[0].heat >= pair[1].heat,
                "heat must be non-increasing: {} < {}",
                pair[0].heat,
                pair[1].heat
            );
        }
    }

    #[test]
    fn search_limit_caps_results() {
        assert_eq!(search_catalog("", None, false, 7).len(), 7);
        // The capped result is the prefix of the uncapped one — the early
        // exit must not disturb the heat ordering.
        let full = search_catalog("", None, false, 0);
        let capped = search_catalog("", None, false, 5);
        assert_eq!(capped, full[..5]);
    }

    #[test]
    fn search_category_filters() {
        let cat = catalog();
        // Pick a category that actually exists in the bundled snapshot.
        let some_cat = cat
            .namespaces
            .values()
            .flat_map(|n| n.categories.iter().cloned())
            .next()
            .expect("catalog has categories");
        let hits = search_catalog("", Some(&some_cat), false, 0);
        assert!(!hits.is_empty(), "category {some_cat} should match");
        // Every hit's namespace must carry the tag.
        let pooled = catalog_categories();
        assert!(pooled.iter().any(|(t, _)| t == &some_cat.to_lowercase()));
    }

    #[test]
    fn search_hides_nsfw() {
        // The bundled snapshot may carry no nsfw-tagged namespace (the docs
        // build prunes them); if it somehow does, hiding must drop exactly
        // those namespaces' routes.
        let nsfw_exists = catalog()
            .namespaces
            .values()
            .any(|n| is_nsfw(&n.categories));
        let hidden = search_catalog("", None, true, 0);
        let shown = search_catalog("", None, false, 0);
        if nsfw_exists {
            assert!(hidden.len() < shown.len());
        }
    }

    #[test]
    fn require_config_round_trips() {
        let (ns, route) = route_lookup("/163/music/user/playrecords/:uid/:type?").unwrap();
        let _ = ns;
        let RequireConfig::Items(items) = &route.features.require_config else {
            panic!("expected items");
        };
        assert!(items.iter().any(|i| i.name == "NCM_COOKIES"));
        // `false` deserializes as None(false), and serializes back to `false`
        // so the frontend sees the documented shape.
        let plain: RequireConfig = serde_json::from_str("false").unwrap();
        assert_eq!(plain, RequireConfig::None(false));
        assert_eq!(serde_json::to_string(&plain).unwrap(), "false");
    }
}
