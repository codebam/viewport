// SPDX-License-Identifier: GPL-3.0-or-later
//
// Policy attached to wlr-layer-shell surfaces.
//
// The protocol gives every surface a namespace but no compositor policy. A
// compiled rule set turns that namespace into renderer-neutral state once when
// the surface arrives and again when configuration reloads. The state lives on
// Smithay's desktop LayerSurface, so clones used by frame assembly all observe
// the same policy without unmapping or rearranging the protocol surface.

use std::cmp::Ordering;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Mutex;

use serde::Deserialize;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::desktop::LayerSurface;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{with_states, RectangleKind, SurfaceAttributes};
use smithay::wayland::shell::wlr_layer::Layer;
use smithay::wayland::shell::xdg::PopupSurface;

const MAX_LAYER_RULES: usize = 256;
const MAX_MATCHER_BYTES: usize = 1024;
const MAX_MATCHER_BYTES_TOTAL: usize = 64 * 1024;
const MAX_COMPILED_REGEX_BYTES: usize = 256 * 1024;
const MAX_HIT_TEST_REGION_OPERATIONS: usize = 1024;

/// One entry in `layer_rules`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleConfig {
    #[serde(rename = "match")]
    pub matcher: MatchConfig,
    /// Final surface-tree alpha, including popups.
    pub opacity: Option<f64>,
    /// Whether screen capture may include this surface tree.
    pub capture: Option<bool>,
    /// Renderer-neutral request for blur behind this surface.
    pub blur: Option<bool>,
    /// Ordering override within the surface's protocol layer.
    pub z_index: Option<i32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MatchConfig {
    pub namespace: Option<NamespaceMatchConfig>,
}

/// The same string matcher shape rich window rules accept.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum NamespaceMatchConfig {
    /// A string is short for a case-insensitive `contains` match.
    Contains(String),
    Rich(NamespaceMatch),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NamespaceMatch {
    pub contains: Option<String>,
    pub equals: Option<String>,
    pub regex: Option<String>,
    pub flags: Option<String>,
}

#[derive(Debug, Clone)]
enum Matcher {
    Contains(String),
    Equals(String),
    Regex { regex: regex::Regex, sticky: bool },
}

impl Matcher {
    fn compile(config: NamespaceMatchConfig) -> anyhow::Result<Self> {
        match config {
            NamespaceMatchConfig::Contains(value) => Ok(Self::Contains(value.to_lowercase())),
            NamespaceMatchConfig::Rich(config) => {
                let operators = usize::from(config.contains.is_some())
                    + usize::from(config.equals.is_some())
                    + usize::from(config.regex.is_some());
                anyhow::ensure!(
                    operators == 1,
                    "exactly one of contains, equals or regex is required"
                );
                anyhow::ensure!(
                    config.regex.is_some() || config.flags.is_none(),
                    "flags require regex"
                );

                if let Some(value) = config.contains {
                    return Ok(Self::Contains(value.to_lowercase()));
                }
                if let Some(value) = config.equals {
                    return Ok(Self::Equals(value.to_lowercase()));
                }

                let pattern = config.regex.expect("one operator was present");
                let mut seen = String::new();
                let mut case_insensitive = false;
                let mut multi_line = false;
                let mut dot_matches_new_line = false;
                let mut sticky = false;
                for flag in config.flags.unwrap_or_default().chars() {
                    anyhow::ensure!(!seen.contains(flag), "duplicate regex flag {flag:?}");
                    seen.push(flag);
                    match flag {
                        'i' => case_insensitive = true,
                        'm' => multi_line = true,
                        's' => dot_matches_new_line = true,
                        // A fresh sticky JavaScript RegExp starts at index 0.
                        'y' => sticky = true,
                        _ => anyhow::bail!("unsupported regex flag {flag:?}"),
                    }
                }
                let mut regex = regex::RegexBuilder::new(&pattern);
                regex
                    .case_insensitive(case_insensitive)
                    .multi_line(multi_line)
                    .dot_matches_new_line(dot_matches_new_line)
                    .size_limit(MAX_COMPILED_REGEX_BYTES)
                    .dfa_size_limit(MAX_COMPILED_REGEX_BYTES);
                Ok(Self::Regex {
                    regex: regex.build()?,
                    sticky,
                })
            }
        }
    }

    fn matches(&self, namespace: &str, lowercase: &str) -> bool {
        match self {
            Self::Contains(wanted) => lowercase.contains(wanted),
            Self::Equals(wanted) => lowercase == wanted,
            Self::Regex { regex, sticky } => regex
                .find(namespace)
                .is_some_and(|matched| !sticky || matched.start() == 0),
        }
    }
}

impl NamespaceMatchConfig {
    fn configured_bytes(&self) -> usize {
        match self {
            Self::Contains(value) => value.len(),
            Self::Rich(config) => {
                config.contains.as_ref().map_or(0, String::len)
                    + config.equals.as_ref().map_or(0, String::len)
                    + config.regex.as_ref().map_or(0, String::len)
                    + config.flags.as_ref().map_or(0, String::len)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Rule {
    matcher: Matcher,
    opacity: Option<f32>,
    capture: Option<bool>,
    blur: Option<bool>,
    z_index: Option<i32>,
}

/// Validated, ordered layer-surface rules.
#[derive(Debug, Clone, Default)]
pub struct Rules(Vec<Rule>);

impl Rules {
    pub fn compile(configured: Vec<RuleConfig>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            configured.len() <= MAX_LAYER_RULES,
            "layer_rules has {} entries; maximum is {MAX_LAYER_RULES}",
            configured.len()
        );
        let mut rules = Vec::with_capacity(configured.len());
        let mut matcher_bytes = 0usize;
        for (at, configured) in configured.into_iter().enumerate() {
            let matcher = configured
                .matcher
                .namespace
                .ok_or_else(|| anyhow::anyhow!("layer_rules[{at}].match.namespace is required"))?;
            let bytes = matcher.configured_bytes();
            anyhow::ensure!(
                bytes <= MAX_MATCHER_BYTES,
                "layer_rules[{at}].match.namespace exceeds {MAX_MATCHER_BYTES} bytes"
            );
            matcher_bytes = matcher_bytes.saturating_add(bytes);
            anyhow::ensure!(
                matcher_bytes <= MAX_MATCHER_BYTES_TOTAL,
                "layer_rules matchers exceed {MAX_MATCHER_BYTES_TOTAL} bytes in total"
            );
            let matcher = Matcher::compile(matcher)
                .map_err(|error| anyhow::anyhow!("layer_rules[{at}].match.namespace: {error}"))?;
            let opacity = configured
                .opacity
                .map(|opacity| {
                    anyhow::ensure!(
                        opacity.is_finite() && (0.0..=1.0).contains(&opacity),
                        "layer_rules[{at}].opacity must be between 0 and 1"
                    );
                    Ok(opacity as f32)
                })
                .transpose()?;
            rules.push(Rule {
                matcher,
                opacity,
                capture: configured.capture,
                blur: configured.blur,
                z_index: configured.z_index,
            });
        }
        Ok(Self(rules))
    }

    /// Compose every matching rule in file order. A later rule replaces only
    /// the fields it names, allowing a broad namespace policy and a narrow
    /// exception without repeating unrelated fields.
    pub fn resolve(&self, namespace: &str) -> Policy {
        let mut policy = Policy::default();
        let lowercase = namespace.to_lowercase();
        for rule in &self.0 {
            if !rule.matcher.matches(namespace, &lowercase) {
                continue;
            }
            if let Some(opacity) = rule.opacity {
                policy.opacity = opacity;
            }
            if let Some(capture) = rule.capture {
                policy.capture = capture;
            }
            if let Some(blur) = rule.blur {
                policy.blur = blur;
            }
            if let Some(z_index) = rule.z_index {
                policy.z_index = z_index;
            }
        }
        policy
    }
}

/// Resolved policy carried by a renderer-neutral frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub opacity: f32,
    pub capture: bool,
    pub blur: bool,
    pub z_index: i32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            capture: true,
            blur: false,
            z_index: 0,
        }
    }
}

struct SurfaceState {
    policy: Mutex<Policy>,
    capture_redaction_id: Id,
    drawable: AtomicBool,
    protocol_layer: Mutex<Layer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HitTestFingerprint {
    view: Option<(Point<i32, Logical>, Size<i32, Logical>)>,
    input_region: Option<RegionFingerprint>,
    popup_geometry: Option<Rectangle<i32, Logical>>,
    overflowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegionFingerprint {
    operations: usize,
    hash: u64,
}

struct SurfaceHitTestState {
    hash_builder: RandomState,
    current: Mutex<Option<HitTestFingerprint>>,
}

impl Default for SurfaceHitTestState {
    fn default() -> Self {
        Self {
            hash_builder: RandomState::new(),
            current: Mutex::new(None),
        }
    }
}

struct SurfaceOwner(Mutex<Option<(Output, LayerSurface)>>);

struct LayerPopup(Mutex<Option<PopupSurface>>);

fn surface_state(layer: &LayerSurface, initial: Policy) -> &SurfaceState {
    layer
        .user_data()
        .insert_if_missing_threadsafe(|| SurfaceState {
            policy: Mutex::new(initial),
            capture_redaction_id: Id::new(),
            drawable: AtomicBool::new(false),
            protocol_layer: Mutex::new(layer.layer()),
        });
    layer
        .user_data()
        .get::<SurfaceState>()
        .expect("inserted above")
}

/// Record whether this tree currently has a root buffer. A transition changes
/// what can receive pointer input under a stationary cursor.
pub fn set_drawable(layer: &LayerSurface, drawable: bool, rules: &Rules) -> bool {
    let state = layer
        .user_data()
        .get::<SurfaceState>()
        .unwrap_or_else(|| surface_state(layer, rules.resolve(layer.namespace())));
    state.drawable.swap(drawable, AtomicOrdering::AcqRel) != drawable
}

/// Record protocol-layer changes independently from geometry arrangement.
pub fn update_protocol_layer(layer: &LayerSurface, rules: &Rules) -> bool {
    let state = layer
        .user_data()
        .get::<SurfaceState>()
        .unwrap_or_else(|| surface_state(layer, rules.resolve(layer.namespace())));
    let mut current = state
        .protocol_layer
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let next = layer.layer();
    let changed = *current != next;
    *current = next;
    changed
}

fn fingerprint_region(
    region: &smithay::wayland::compositor::RegionAttributes,
    hash_builder: &RandomState,
) -> (Option<RegionFingerprint>, bool) {
    if region.rects.len() > MAX_HIT_TEST_REGION_OPERATIONS {
        return (None, true);
    }
    let mut hasher = hash_builder.build_hasher();
    for (kind, rect) in &region.rects {
        matches!(kind, RectangleKind::Add).hash(&mut hasher);
        (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h).hash(&mut hasher);
    }
    (
        Some(RegionFingerprint {
            operations: region.rects.len(),
            hash: hasher.finish(),
        }),
        false,
    )
}

/// Track only one committed surface's hit-test state. Oversized client regions
/// force a conservative refresh without being copied into compositor memory.
pub fn hit_test_state_changed(
    surface: &WlSurface,
    popup_geometry: Option<Rectangle<i32, Logical>>,
) -> bool {
    with_states(surface, |states| {
        let state = states
            .data_map
            .get_or_insert_threadsafe(SurfaceHitTestState::default);
        let view = states
            .data_map
            .get::<RendererSurfaceStateUserData>()
            .and_then(|data| {
                data.lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .view()
            })
            .map(|view| (view.offset, view.dst));
        let (input_region, overflowed) = if states.cached_state.has::<SurfaceAttributes>() {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .input_region
                .as_ref()
                .map(|region| fingerprint_region(region, &state.hash_builder))
                .unwrap_or((None, false))
        } else {
            (None, false)
        };
        let next = HitTestFingerprint {
            view,
            input_region,
            popup_geometry,
            overflowed,
        };
        let mut current = state
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let changed = next.overflowed || current.as_ref() != Some(&next);
        *current = Some(next);
        changed
    })
}

fn set_surface_owner(surface: &WlSurface, owner: Option<(Output, LayerSurface)>) {
    with_states(surface, |states| {
        let state = states
            .data_map
            .get_or_insert_threadsafe(|| SurfaceOwner(Mutex::new(None)));
        *state.0.lock().unwrap_or_else(|error| error.into_inner()) = owner;
    });
}

pub fn set_owner(layer: &LayerSurface, output: Output) {
    set_surface_owner(layer.wl_surface(), Some((output, layer.clone())));
}

pub fn inherit_owner(surface: &WlSurface, parent: &WlSurface) -> bool {
    let Some(owner) = owner(parent) else {
        return false;
    };
    set_surface_owner(surface, Some(owner));
    true
}

pub fn owner(surface: &WlSurface) -> Option<(Output, LayerSurface)> {
    with_states(surface, |states| {
        let state = states.data_map.get::<SurfaceOwner>()?;
        state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    })
}

pub fn clear_owner(surface: &WlSurface) -> bool {
    let had_owner = owner(surface).is_some();
    if had_owner {
        set_surface_owner(surface, None);
    }
    had_owner
}

pub fn register_popup(surface: &PopupSurface) {
    with_states(surface.wl_surface(), |states| {
        let state = states
            .data_map
            .get_or_insert_threadsafe(|| LayerPopup(Mutex::new(None)));
        *state.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(surface.clone());
    });
}

pub fn clear_popup(surface: &WlSurface) {
    with_states(surface, |states| {
        let Some(state) = states.data_map.get::<LayerPopup>() else {
            return;
        };
        *state.0.lock().unwrap_or_else(|error| error.into_inner()) = None;
    });
}

pub fn popup_geometry(surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
    let popup = with_states(surface, |states| {
        states.data_map.get::<LayerPopup>().and_then(|state| {
            state
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        })
    })?;
    popup.with_committed_state(|state| state.map(|state| state.geometry))
}

/// Resolve and atomically replace one mapped surface's current policy.
pub fn apply(layer: &LayerSurface, rules: &Rules) -> (Policy, bool) {
    let resolved = rules.resolve(layer.namespace());
    let state = surface_state(layer, resolved);
    let mut current = state
        .policy
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let changed = *current != resolved;
    *current = resolved;
    (resolved, changed)
}

pub struct Snapshot {
    pub policy: Policy,
    pub capture_redaction_id: Id,
}

/// Read only current policy, for hot paths such as pointer hit testing.
pub fn policy(layer: &LayerSurface, rules: &Rules) -> Policy {
    let state = layer
        .user_data()
        .get::<SurfaceState>()
        .unwrap_or_else(|| surface_state(layer, rules.resolve(layer.namespace())));
    *state
        .policy
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Read policy and stable renderer identity without borrowing compositor state.
/// A missing attachment is initialized from the current rules, so capture can
/// never fall back to an unrelated permissive default.
pub fn snapshot(layer: &LayerSurface, rules: &Rules) -> Snapshot {
    let state = layer
        .user_data()
        .get::<SurfaceState>()
        .unwrap_or_else(|| surface_state(layer, rules.resolve(layer.namespace())));
    let policy = *state
        .policy
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    Snapshot {
        policy,
        capture_redaction_id: state.capture_redaction_id.clone(),
    }
}

fn protocol_rank(layer: Layer) -> u8 {
    match layer {
        Layer::Background => 0,
        Layer::Bottom => 1,
        Layer::Top => 2,
        Layer::Overlay => 3,
    }
}

/// Front-to-back ordering. `z_index` applies only after protocol layer, so no
/// override can lift a background surface over a top or overlay surface. Equal
/// values retain wlr-layer-shell's newest-mapped-on-top order.
pub fn stacking_order(left: (Layer, i32, usize), right: (Layer, i32, usize)) -> Ordering {
    protocol_rank(right.0)
        .cmp(&protocol_rank(left.0))
        .then_with(|| right.1.cmp(&left.1))
        .then_with(|| right.2.cmp(&left.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(json: serde_json::Value) -> Rules {
        let configured: Vec<RuleConfig> = serde_json::from_value(json).expect("rule syntax");
        Rules::compile(configured).expect("valid rules")
    }

    #[test]
    fn namespace_matchers_follow_rich_window_rule_forms() {
        let contains = rules(serde_json::json!([
            {"match": {"namespace": "BAR"}, "blur": true}
        ]));
        assert!(contains.resolve("waybar").blur);

        let exact = rules(serde_json::json!([
            {"match": {"namespace": {"equals": "WAYBAR"}}, "capture": false}
        ]));
        assert!(!exact.resolve("waybar").capture);
        assert!(exact.resolve("waybar-secondary").capture);

        let regex = rules(serde_json::json!([
            {"match": {"namespace": {"regex": "^panel-[0-9]+$", "flags": "i"}},
             "opacity": 0.75}
        ]));
        assert_eq!(regex.resolve("PANEL-12").opacity, 0.75);
        assert_eq!(regex.resolve("panel-x").opacity, 1.0);

        let sticky = rules(serde_json::json!([
            {"match": {"namespace": {"regex": "panel", "flags": "y"}}, "blur": true}
        ]));
        assert!(sticky.resolve("panel-menu").blur);
        assert!(!sticky.resolve("private-panel").blur);

        let sticky_extended = rules(serde_json::json!([
            {"match": {"namespace": {"regex": "(?x)panel # comment", "flags": "y"}},
             "blur": true}
        ]));
        assert!(sticky_extended.resolve("panel-menu").blur);
    }

    #[test]
    fn matching_rules_compose_in_order_by_explicit_field() {
        let rules = rules(serde_json::json!([
            {"match": {"namespace": {"contains": "bar"}},
             "opacity": 0.8, "capture": false, "z_index": 2},
            {"match": {"namespace": {"equals": "waybar"}},
             "opacity": 0.6, "blur": true},
            {"match": {"namespace": {"regex": "^way", "flags": "i"}},
             "capture": true, "z_index": 7}
        ]));

        assert_eq!(
            rules.resolve("waybar"),
            Policy {
                opacity: 0.6,
                capture: true,
                blur: true,
                z_index: 7,
            }
        );
        assert_eq!(
            rules.resolve("statusbar"),
            Policy {
                opacity: 0.8,
                capture: false,
                blur: false,
                z_index: 2,
            }
        );
        assert_eq!(rules.resolve("launcher"), Policy::default());
    }

    #[test]
    fn invalid_or_ambiguous_matchers_are_rejected() {
        let misspelled = serde_json::json!([
            {"match": {"namespace": "bar"}, "caputre": false}
        ]);
        assert!(serde_json::from_value::<Vec<RuleConfig>>(misspelled).is_err());

        let invalid: Vec<RuleConfig> = serde_json::from_value(serde_json::json!([
            {"match": {"namespace": {"regex": "("}}, "capture": false}
        ]))
        .unwrap();
        assert!(Rules::compile(invalid).is_err());

        let ambiguous: Vec<RuleConfig> = serde_json::from_value(serde_json::json!([
            {"match": {"namespace": {"contains": "bar", "equals": "bar"}}}
        ]))
        .unwrap();
        assert!(Rules::compile(ambiguous).is_err());

        let invalid_opacity: Vec<RuleConfig> = serde_json::from_value(serde_json::json!([
            {"match": {"namespace": "bar"}, "opacity": 1.1}
        ]))
        .unwrap();
        assert!(Rules::compile(invalid_opacity).is_err());

        let incompatible_flags: Vec<RuleConfig> = serde_json::from_value(serde_json::json!([
            {"match": {"namespace": {"regex": "bar", "flags": "uv"}}}
        ]))
        .unwrap();
        assert!(Rules::compile(incompatible_flags).is_err());
    }

    #[test]
    fn matcher_resource_limits_are_enforced() {
        let rule = |matcher| RuleConfig {
            matcher: MatchConfig {
                namespace: Some(matcher),
            },
            ..RuleConfig::default()
        };

        let ordinary = rule(NamespaceMatchConfig::Contains("panel".to_owned()));
        assert!(Rules::compile(vec![ordinary; MAX_LAYER_RULES + 1]).is_err());
        assert!(Rules::compile(vec![rule(NamespaceMatchConfig::Contains(
            "x".repeat(MAX_MATCHER_BYTES + 1)
        ))])
        .is_err());

        let aggregate = rule(NamespaceMatchConfig::Contains(
            "x".repeat(MAX_MATCHER_BYTES),
        ));
        assert!(Rules::compile(vec![
            aggregate;
            MAX_MATCHER_BYTES_TOTAL / MAX_MATCHER_BYTES + 1
        ])
        .is_err());

        assert!(
            Rules::compile(vec![rule(NamespaceMatchConfig::Rich(NamespaceMatch {
                regex: Some("panel".to_owned()),
                flags: Some("d".to_owned()),
                ..NamespaceMatch::default()
            }))])
            .is_err()
        );
    }

    #[test]
    fn hit_test_region_fingerprints_are_bounded() {
        let hash_builder = RandomState::new();
        let bounded = smithay::wayland::compositor::RegionAttributes {
            rects: (0..MAX_HIT_TEST_REGION_OPERATIONS)
                .map(|x| {
                    (
                        RectangleKind::Add,
                        Rectangle::new((x as i32, 0).into(), (1, 1).into()),
                    )
                })
                .collect(),
        };
        let (fingerprint, overflowed) = fingerprint_region(&bounded, &hash_builder);
        assert!(!overflowed);
        assert_eq!(
            fingerprint.map(|fingerprint| fingerprint.operations),
            Some(MAX_HIT_TEST_REGION_OPERATIONS)
        );

        let oversized = smithay::wayland::compositor::RegionAttributes {
            rects: (0..=MAX_HIT_TEST_REGION_OPERATIONS)
                .map(|x| {
                    (
                        RectangleKind::Add,
                        Rectangle::new((x as i32, 0).into(), (1, 1).into()),
                    )
                })
                .collect(),
        };
        assert_eq!(fingerprint_region(&oversized, &hash_builder), (None, true));
    }

    #[test]
    fn z_index_never_crosses_protocol_layers_and_ties_are_stable() {
        let mut layers = [
            (Layer::Top, 0, 0, "old top"),
            (Layer::Overlay, -100, 0, "overlay"),
            (Layer::Top, 0, 1, "new top"),
            (Layer::Top, 5, 0, "raised top"),
            (Layer::Bottom, i32::MAX, 0, "bottom"),
        ];
        layers.sort_by(|left, right| {
            stacking_order((left.0, left.1, left.2), (right.0, right.1, right.2))
        });

        assert_eq!(
            layers.map(|layer| layer.3),
            ["overlay", "raised top", "new top", "old top", "bottom"]
        );
    }
}
