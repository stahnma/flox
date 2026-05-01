use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

use anyhow::Result;
use base64::Engine as _;
use flox_core::activate::context::{AttachCtx, AttachProjectCtx};
use serde::{Deserialize, Serialize};

use crate::activate_script_builder::{collect_activate_exports, old_cli_envs};
use crate::env_diff::EnvDiff;
use crate::vars_from_env::VarsFromEnvironment;

pub const FLOX_HOOK_DIFF_VAR: &str = "_FLOX_HOOK_DIFF";

/// The diff between the pre-activation shell environment and the intended
/// post-activation environment, captured at attach time.
///
/// Each category stores the *original* value (for deactivation purposes),
/// except for `added` which stores the new value (since there is no original).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivationDiff {
    /// Vars newly set by activation (stores new value).
    pub added: HashMap<String, String>,
    /// Vars whose value will change (stores *original* value).
    pub modified: HashMap<String, String>,
    /// Vars that will be unset (stores *original* value).
    pub removed: HashMap<String, String>,
}

/// Compute the diff between the current environment and the intended
/// post-activation state.
///
/// `intended_sets` and `intended_removals` are pre-assembled by the caller.
/// When a key appears in both, removal wins (it is excluded from the diff's
/// `added`/`modified` categories and placed in `removed` if it existed).
fn diff_env(
    current_env: &HashMap<String, String>,
    intended_sets: &HashMap<String, String>,
    intended_removals: &HashSet<String>,
) -> ActivationDiff {
    // Removal overrides addition: drop any set that is also scheduled for removal.
    let sets_without_overrides: HashMap<&String, &String> = intended_sets
        .iter()
        .filter(|(k, _)| !intended_removals.contains(*k))
        .collect();

    let mut added = HashMap::new();
    let mut modified = HashMap::new();
    let mut removed = HashMap::new();

    for (k, new_val) in &sets_without_overrides {
        match current_env.get(*k) {
            None => {
                // Key not in current env: it will be newly added.
                added.insert((*k).clone(), (*new_val).clone());
            },
            Some(old_val) if old_val != *new_val => {
                // Key exists but value will change: store original value.
                modified.insert((*k).clone(), old_val.clone());
            },
            Some(_) => {
                // Value unchanged: not part of the diff.
            },
        }
    }

    for k in intended_removals {
        if let Some(old_val) = current_env.get(k) {
            // Key is in current env and will be removed: store original value.
            removed.insert(k.clone(), old_val.clone());
        }
    }

    ActivationDiff {
        added,
        modified,
        removed,
    }
}

impl ActivationDiff {
    /// Compute the diff given a snapshot of the current environment and all
    /// the activation parameters.
    ///
    /// `current_env` is the environment *before* activation.
    pub fn compute_from_snapshot(
        current_env: &HashMap<String, String>,
        context: &AttachCtx,
        project: Option<&AttachProjectCtx>,
        subsystem_verbosity: u32,
        vars_from_env: VarsFromEnvironment,
        env_diff: &EnvDiff,
    ) -> Self {
        // Collect all intended sets by merging (later overrides earlier).
        let mut intended_sets: HashMap<String, String> = HashMap::new();

        // 1. old_cli_envs: HashMap<&'static str, String>
        for (k, v) in old_cli_envs(context, project) {
            intended_sets.insert(k.to_string(), v);
        }

        // 2. collect_activate_exports: (HashMap<&'static str, String>, Vec<&'static str>)
        let (export_map, removal_list) =
            collect_activate_exports(context, project, subsystem_verbosity, vars_from_env);
        for (k, v) in export_map {
            intended_sets.insert(k.to_string(), v);
        }

        // 3. env_diff.additions: HashMap<String, String>
        for (k, v) in &env_diff.additions {
            intended_sets.insert(k.clone(), v.clone());
        }

        // Collect all intended removals.
        let mut intended_removals: HashSet<String> = HashSet::new();

        // From collect_activate_exports removal list.
        for k in &removal_list {
            intended_removals.insert(k.to_string());
        }

        // From env_diff.deletions.
        for k in &env_diff.deletions {
            intended_removals.insert(k.clone());
        }

        diff_env(current_env, &intended_sets, &intended_removals)
    }

    /// Convenience wrapper that extracts the full env snapshot from
    /// `VarsFromEnvironment` and delegates to `compute_from_snapshot`.
    ///
    /// Returns an empty diff if `full_env` is not populated.
    pub fn compute(
        vars_from_env: &VarsFromEnvironment,
        context: &AttachCtx,
        project: Option<&AttachProjectCtx>,
        subsystem_verbosity: u32,
        vars_from_env_for_exports: VarsFromEnvironment,
        env_diff: &EnvDiff,
    ) -> Self {
        let current_env = match &vars_from_env.full_env {
            Some(env) => env,
            None => {
                return Self {
                    added: HashMap::new(),
                    modified: HashMap::new(),
                    removed: HashMap::new(),
                };
            },
        };
        Self::compute_from_snapshot(
            current_env,
            context,
            project,
            subsystem_verbosity,
            vars_from_env_for_exports,
            env_diff,
        )
    }

    /// Serialize to zlib-compressed base64url JSON.
    pub fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self)?;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&json)?;
        let compressed = encoder.finish()?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&compressed))
    }

    /// Deserialize from zlib-compressed base64url JSON.
    ///
    /// Used for deactivation to restore the original environment.
    pub fn decode(encoded: &str) -> Result<Self> {
        let compressed = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded)?;
        let mut decoder = flate2::read::ZlibDecoder::new(&compressed[..]);
        let mut json = Vec::new();
        decoder.read_to_end(&mut json)?;
        Ok(serde_json::from_slice(&json)?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn make_env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn make_removals(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn test_compute_additions() {
        let current = make_env(&[("EXISTING", "value")]);
        let sets = make_env(&[("NEW_VAR", "new_value")]);
        let diff = diff_env(&current, &sets, &make_removals(&[]));

        assert_eq!(diff.added, make_env(&[("NEW_VAR", "new_value")]));
        assert!(diff.modified.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn test_compute_modifications() {
        let current = make_env(&[("MY_VAR", "old_value")]);
        let sets = make_env(&[("MY_VAR", "new_value")]);
        let diff = diff_env(&current, &sets, &make_removals(&[]));

        // modified stores original value
        assert!(diff.added.is_empty());
        assert_eq!(diff.modified, make_env(&[("MY_VAR", "old_value")]));
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn test_compute_removals() {
        let current = make_env(&[("GONE_VAR", "gone_value")]);
        let diff = diff_env(&current, &HashMap::new(), &make_removals(&["GONE_VAR"]));

        // removed stores original value
        assert!(diff.added.is_empty());
        assert!(diff.modified.is_empty());
        assert_eq!(diff.removed, make_env(&[("GONE_VAR", "gone_value")]));
    }

    #[test]
    fn test_compute_mixed() {
        let current = make_env(&[("MODIFIED_VAR", "orig"), ("REMOVED_VAR", "to_remove")]);
        let sets = make_env(&[("NEW_VAR", "new"), ("MODIFIED_VAR", "changed")]);
        let diff = diff_env(&current, &sets, &make_removals(&["REMOVED_VAR"]));

        assert_eq!(diff.added, make_env(&[("NEW_VAR", "new")]));
        assert_eq!(diff.modified, make_env(&[("MODIFIED_VAR", "orig")]));
        assert_eq!(diff.removed, make_env(&[("REMOVED_VAR", "to_remove")]));
    }

    #[test]
    fn test_deletion_overrides_addition() {
        // A var that appears in both intended_sets and intended_removals should
        // end up in the removed category only (removal wins).
        let current = make_env(&[("CONFLICT_VAR", "current_value")]);
        let sets = make_env(&[("CONFLICT_VAR", "new_value")]);
        let diff = diff_env(&current, &sets, &make_removals(&["CONFLICT_VAR"]));

        assert!(diff.added.is_empty());
        assert!(diff.modified.is_empty());
        assert_eq!(diff.removed, make_env(&[("CONFLICT_VAR", "current_value")]));
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let original = ActivationDiff {
            added: make_env(&[("NEW_VAR", "new_value")]),
            modified: make_env(&[("MOD_VAR", "original_value")]),
            removed: make_env(&[("REM_VAR", "removed_value")]),
        };

        let encoded = original.encode().expect("encode should succeed");
        let decoded = ActivationDiff::decode(&encoded).expect("decode should succeed");

        assert_eq!(original, decoded);
    }

    #[test]
    fn test_no_changes_empty_diff() {
        let current = make_env(&[("UNCHANGED", "value")]);
        // Sets contain the same key/value as current, and no removals.
        let sets = make_env(&[("UNCHANGED", "value")]);
        let diff = diff_env(&current, &sets, &make_removals(&[]));

        assert!(diff.added.is_empty());
        assert!(diff.modified.is_empty());
        assert!(diff.removed.is_empty());
    }
}
