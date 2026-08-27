// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet};
use viewport_ipc::event::VrrMode;

pub fn vrr_effective(mode: VrrMode, fullscreen: bool, game_or_video: bool) -> bool {
    match mode {
        VrrMode::Off => false,
        VrrMode::Always => true,
        VrrMode::Fullscreen => fullscreen,
        VrrMode::GameOrVideo => game_or_video,
    }
}

/// Validate direct physical-head mirroring. Sources must be real, independent
/// heads on the same GPU; disallowing source-as-sink also disallows chains and
/// cycles without needing renderer recursion.
pub fn validate(
    mirrors: &HashMap<String, String>,
    present: &HashSet<String>,
    gpu: impl Fn(&str) -> Option<usize>,
) -> Result<(), String> {
    for (sink, source) in mirrors {
        if sink == source {
            return Err(format!("{sink} cannot mirror itself"));
        }
        if !present.contains(sink) {
            return Err(format!("mirror sink {sink} does not exist"));
        }
        if !present.contains(source) {
            return Err(format!("mirror source {source} does not exist"));
        }
        if mirrors.contains_key(source) {
            return Err(format!("mirror chain {sink} -> {source} is not supported"));
        }
        if gpu(sink) != gpu(source) {
            return Err(format!("{sink} and {source} are on different GPUs"));
        }
    }
    Ok(())
}

/// Remove a physical head deterministically. If it was a source, the
/// lexicographically first surviving sink becomes the desktop and remaining
/// sinks mirror it. One logical desktop therefore survives a source unplug.
pub fn remove(
    mirrors: &mut HashMap<String, String>,
    gone: &str,
    enabled: &HashSet<String>,
) -> Option<String> {
    mirrors.remove(gone);
    let sinks: Vec<String> = mirrors
        .iter()
        .filter(|(_, source)| source.as_str() == gone)
        .map(|(sink, _)| sink.clone())
        .collect();
    let promoted = sinks
        .iter()
        .filter(|sink| enabled.contains(*sink))
        .min()
        .cloned();
    for sink in sinks {
        if Some(&sink) == promoted.as_ref() {
            mirrors.remove(&sink);
        } else if let Some(source) = promoted.as_ref() {
            mirrors.insert(sink, source.clone());
        } else {
            mirrors.remove(&sink);
        }
    }
    promoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> HashSet<String> {
        ["A", "B", "C"].into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn rejects_self_missing_chains_and_cross_gpu() {
        let mut map = HashMap::from([("A".into(), "A".into())]);
        assert!(validate(&map, &names(), |_| Some(0))
            .unwrap_err()
            .contains("itself"));
        map = HashMap::from([("A".into(), "gone".into())]);
        assert!(validate(&map, &names(), |_| Some(0))
            .unwrap_err()
            .contains("does not exist"));
        map = HashMap::from([("A".into(), "B".into()), ("B".into(), "C".into())]);
        assert!(validate(&map, &names(), |_| Some(0))
            .unwrap_err()
            .contains("chain"));
        map = HashMap::from([("A".into(), "B".into())]);
        assert!(
            validate(&map, &names(), |name| Some(usize::from(name == "B")))
                .unwrap_err()
                .contains("different GPUs")
        );
    }

    #[test]
    fn source_removal_promotes_stable_sink() {
        let mut map = HashMap::from([("C".into(), "A".into()), ("B".into(), "A".into())]);
        assert_eq!(remove(&mut map, "A", &names()).as_deref(), Some("B"));
        assert_eq!(map, HashMap::from([("C".into(), "B".into())]));
    }

    #[test]
    fn source_removal_never_promotes_a_disabled_sink() {
        let mut map = HashMap::from([("C".into(), "A".into()), ("B".into(), "A".into())]);
        assert_eq!(remove(&mut map, "A", &HashSet::new()), None);
        assert!(map.is_empty());
    }

    #[test]
    fn source_removal_prefers_an_enabled_sink() {
        let mut map = HashMap::from([("B".into(), "A".into()), ("C".into(), "A".into())]);
        assert_eq!(
            remove(&mut map, "A", &HashSet::from(["C".into()])).as_deref(),
            Some("C")
        );
        assert_eq!(map, HashMap::from([("B".into(), "C".into())]));
    }

    #[test]
    fn vrr_modes_are_conservative_and_independent() {
        assert!(!vrr_effective(VrrMode::Off, true, true));
        assert!(vrr_effective(VrrMode::Always, false, false));
        assert!(vrr_effective(VrrMode::Fullscreen, true, false));
        assert!(!vrr_effective(VrrMode::Fullscreen, false, true));
        assert!(vrr_effective(VrrMode::GameOrVideo, false, true));
        assert!(!vrr_effective(VrrMode::GameOrVideo, true, false));
    }
}
