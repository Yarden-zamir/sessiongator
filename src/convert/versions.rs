//! Version policy for native conversion. The manifest
//! `docs/specs/native-session-import-versions.toml` is compiled in; the
//! statuses are explained in `docs/specs/native-session-import.md`
//! ("Version Policy").

use std::{fmt, process::Command, sync::LazyLock};

use figment::{
    providers::{Format, Toml},
    Figment,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::model::Tool;

const MANIFEST_TOML: &str = include_str!("../../docs/specs/native-session-import-versions.toml");

/// A dotted numeric version such as `2.1.299`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(Vec<u64>);

impl Version {
    pub fn parse(value: &str) -> Option<Self> {
        let parts: Option<Vec<u64>> = value
            .trim_start_matches('v')
            .split('.')
            .map(|part| part.parse().ok())
            .collect();
        parts.filter(|parts| !parts.is_empty()).map(Self)
    }

    fn major(&self) -> u64 {
        self.0[0]
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u64::to_string).collect();
        formatter.write_str(&parts.join("."))
    }
}

/// A manifest status. `Bracketed` and `Untested` are derived, never written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Recorded {
    /// A full pass with real clients and real model keys: the target tool
    /// resumed an imported session and answered from its history.
    Verified,
    /// A CI round trip without model calls: sessiongator wrote and read back
    /// the session, and the tool listed, exported, or parsed it.
    ProbePassed,
    /// Sessions written by these versions were read. Source only.
    ReadObserved,
}

#[derive(Deserialize)]
struct ManifestFile {
    tools: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    tool: Tool,
    version: Option<String>,
    version_range: Option<String>,
    status: Recorded,
}

struct Entry {
    tool: Tool,
    low: Version,
    high: Version,
    status: Recorded,
}

static MANIFEST: LazyLock<Vec<Entry>> = LazyLock::new(|| {
    let file: ManifestFile = Figment::from(Toml::string(MANIFEST_TOML))
        .extract()
        .expect("native-session-import-versions.toml must parse");
    file.tools.into_iter().map(entry).collect()
});

/// A broken manifest is a build defect, so it panics; `manifest_parses`
/// catches it in tests.
fn entry(raw: RawEntry) -> Entry {
    let (low, high) = match (&raw.version, &raw.version_range, raw.status) {
        (Some(version), None, Recorded::Verified | Recorded::ProbePassed) => {
            let version = Version::parse(version).expect("manifest version must be dotted numbers");
            (version.clone(), version)
        }
        (None, Some(range), Recorded::ReadObserved) => {
            let (low, high) = range
                .split_once("..=")
                .expect("manifest version_range must look like 1.2.3..=1.4.0");
            (
                Version::parse(low).expect("manifest range start must be dotted numbers"),
                Version::parse(high).expect("manifest range end must be dotted numbers"),
            )
        }
        _ => panic!(
            "manifest entry for {} needs `version` with verified/probe-passed or `version_range` with read-observed",
            raw.tool.name()
        ),
    };
    Entry {
        tool: raw.tool,
        low,
        high,
        status: raw.status,
    }
}

fn recorded(tool: Tool, status: Recorded) -> Vec<&'static Version> {
    let mut versions: Vec<&Version> = MANIFEST
        .iter()
        .filter(|entry| entry.tool == tool && entry.status == status)
        .map(|entry| &entry.low)
        .collect();
    versions.sort();
    versions.dedup();
    versions
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Source,
    Target,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Support {
    Verified,
    /// Between two verified versions of the same major version. Treated as
    /// verified: the store format did not change across the verified ends.
    Bracketed {
        low: String,
        high: String,
    },
    ProbePassed,
    /// Source only: in a `read-observed` range.
    ReadObserved,
    /// Same major version as a known version and not older than the oldest
    /// one. Tool releases ship often without store format changes, so these
    /// convert with a warning. Revisit if a minor or patch release breaks a
    /// store format: record a per-tool breaking version then.
    Untested {
        nearest: String,
    },
    Unsupported {
        reason: String,
    },
}

impl Support {
    pub fn trusted(&self) -> bool {
        matches!(
            self,
            Self::Verified | Self::Bracketed { .. } | Self::ReadObserved
        )
    }

    pub fn label(&self) -> String {
        match self {
            Self::Verified => "verified".to_string(),
            Self::Bracketed { low, high } => format!("bracketed by verified {low} and {high}"),
            Self::ProbePassed => "probe-passed, no full pass yet".to_string(),
            Self::ReadObserved => "read-observed".to_string(),
            Self::Untested { nearest } => format!("untested, nearest known {nearest}"),
            Self::Unsupported { reason } => format!("unsupported: {reason}"),
        }
    }

    pub fn json(&self) -> Value {
        match self {
            Self::Verified => json!({ "status": "verified" }),
            Self::Bracketed { low, high } => {
                json!({ "status": "bracketed", "low": low, "high": high })
            }
            Self::ProbePassed => json!({ "status": "probe-passed" }),
            Self::ReadObserved => json!({ "status": "read-observed" }),
            Self::Untested { nearest } => json!({ "status": "untested", "nearest": nearest }),
            Self::Unsupported { reason } => json!({ "status": "unsupported", "reason": reason }),
        }
    }
}

pub fn support(tool: Tool, version: Option<&str>, role: Role) -> Support {
    let Some(raw) = version else {
        return Support::Unsupported {
            reason: format!(
                "cannot detect the {0} version (is {0} on PATH?)",
                tool.name()
            ),
        };
    };
    let Some(version) = Version::parse(raw) else {
        return Support::Unsupported {
            reason: format!("cannot parse version {raw:?}"),
        };
    };
    let verified = recorded(tool, Recorded::Verified);
    if verified.contains(&&version) {
        return Support::Verified;
    }
    let same_major = |known: &&&Version| known.major() == version.major();
    let below = verified
        .iter()
        .filter(same_major)
        .filter(|known| ***known < version)
        .max();
    let above = verified
        .iter()
        .filter(same_major)
        .filter(|known| ***known > version)
        .min();
    if let (Some(low), Some(high)) = (below, above) {
        return Support::Bracketed {
            low: low.to_string(),
            high: high.to_string(),
        };
    }
    let probed = recorded(tool, Recorded::ProbePassed);
    if probed.contains(&&version) {
        return Support::ProbePassed;
    }
    if role == Role::Source
        && MANIFEST.iter().any(|entry| {
            entry.tool == tool
                && entry.status == Recorded::ReadObserved
                && entry.low <= version
                && version <= entry.high
        })
    {
        return Support::ReadObserved;
    }
    let known: Vec<&Version> = verified
        .iter()
        .chain(probed.iter())
        .copied()
        .filter(|known| known.major() == version.major())
        .collect();
    match known.iter().filter(|known| ***known <= version).max() {
        Some(nearest) => Support::Untested {
            nearest: nearest.to_string(),
        },
        None => Support::Unsupported {
            reason: match known.iter().min() {
                Some(oldest) => format!(
                    "older than {oldest}, the oldest known {}.x version",
                    version.major()
                ),
                None => format!("no known {} {}.x version", tool.name(), version.major()),
            },
        },
    }
}

/// The newest verified or probe-passed version: the version stamp for a
/// written session when the tool is not installed.
pub fn newest_known(tool: Tool) -> String {
    recorded(tool, Recorded::Verified)
        .into_iter()
        .chain(recorded(tool, Recorded::ProbePassed))
        .max()
        .map(Version::to_string)
        .expect("the manifest must list a version for every tool")
}

/// One line per status for `sessiongator compat`.
pub fn describe(tool: Tool) -> Vec<String> {
    let join = |versions: Vec<&Version>| {
        versions
            .iter()
            .map(|version| version.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let verified = recorded(tool, Recorded::Verified);
    let brackets: Vec<String> = verified
        .windows(2)
        .filter(|pair| pair[0].major() == pair[1].major())
        .map(|pair| format!("{}..{}", pair[0], pair[1]))
        .collect();
    let read_observed: Vec<String> = MANIFEST
        .iter()
        .filter(|entry| entry.tool == tool && entry.status == Recorded::ReadObserved)
        .map(|entry| format!("{}..={}", entry.low, entry.high))
        .collect();
    let mut lines = Vec::new();
    let mut push = |label: &str, value: String| {
        if !value.is_empty() {
            lines.push(format!("{label:<14}{value}"));
        }
    };
    push("verified", join(verified.clone()));
    push("bracketed", brackets.join(", "));
    push("probe-passed", join(recorded(tool, Recorded::ProbePassed)));
    push("read-observed", read_observed.join(", "));
    lines
}

pub fn detect_cli_version(tool: Tool) -> Option<String> {
    let output = Command::new(tool.name()).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_command_version(&String::from_utf8_lossy(&output.stdout))
}

/// First token that looks like a dotted version, without a `v` prefix or a
/// trailing period. Covers `2.1.299 (Claude Code)`, `opencode v2.0.28`,
/// `codex-cli 0.158.0`, and `GitHub Copilot CLI 1.0.97.`.
fn parse_command_version(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        .map(|token| token.trim_end_matches('.'))
        .filter(|token| token.contains('.'))
        .find_map(Version::parse)
        .map(|version| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bump(version: &Version, by: i64) -> String {
        let mut parts = version.0.clone();
        let last = parts.last_mut().unwrap();
        *last = (*last as i64 + by) as u64;
        Version(parts).to_string()
    }

    #[test]
    fn manifest_parses_and_lists_every_tool() {
        for tool in Tool::ALL {
            assert!(Version::parse(&newest_known(tool)).is_some());
        }
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(Version::parse("2.1.100") > Version::parse("2.1.99"));
        assert_eq!(Version::parse("v2.0.28"), Version::parse("2.0.28"));
        assert_eq!(Version::parse("2.x"), None);
        assert_eq!(Version::parse(""), None);
    }

    #[test]
    fn recorded_versions_keep_their_status() {
        for tool in Tool::ALL {
            for version in recorded(tool, Recorded::Verified) {
                assert_eq!(
                    support(tool, Some(&version.to_string()), Role::Target),
                    Support::Verified
                );
            }
            for version in recorded(tool, Recorded::ProbePassed) {
                let support = support(tool, Some(&version.to_string()), Role::Target);
                assert!(
                    matches!(support, Support::ProbePassed | Support::Bracketed { .. }),
                    "{} {version}: {support:?}",
                    tool.name()
                );
            }
        }
    }

    #[test]
    fn versions_between_two_verified_versions_are_bracketed() {
        for tool in Tool::ALL {
            let verified = recorded(tool, Recorded::Verified);
            for pair in verified
                .windows(2)
                .filter(|pair| pair[0].major() == pair[1].major())
            {
                let inside = bump(pair[0], 1);
                if Version::parse(&inside).as_ref() == Some(pair[1]) {
                    continue;
                }
                let support = support(tool, Some(&inside), Role::Target);
                assert!(support.trusted(), "{} {inside}: {support:?}", tool.name());
            }
        }
    }

    #[test]
    fn newer_versions_in_a_known_major_line_are_untested_not_blocked() {
        for tool in Tool::ALL {
            let newest = Version::parse(&newest_known(tool)).unwrap();
            let support = support(tool, Some(&bump(&newest, 1)), Role::Target);
            assert_eq!(
                support,
                Support::Untested {
                    nearest: newest.to_string()
                },
                "{}",
                tool.name()
            );
            assert!(!support.trusted());
        }
    }

    #[test]
    fn unknown_major_old_and_undetected_versions_are_unsupported() {
        for version in [None, Some("99.0.0"), Some("0.0.1"), Some("garbage")] {
            assert!(
                matches!(
                    support(Tool::Claude, version, Role::Target),
                    Support::Unsupported { .. }
                ),
                "{version:?}"
            );
        }
    }

    #[test]
    fn opencode_1_is_a_readable_source_but_not_a_target() {
        assert_eq!(
            support(Tool::Opencode, Some("1.17.10"), Role::Source),
            Support::ReadObserved
        );
        for version in ["1.17.10", "1.18.40"] {
            assert!(matches!(
                support(Tool::Opencode, Some(version), Role::Target),
                Support::Unsupported { .. }
            ));
        }
    }

    #[test]
    fn parses_cli_version_output_of_every_tool() {
        for (stdout, expected) in [
            ("2.1.299 (Claude Code)\n", "2.1.299"),
            ("opencode v2.0.28\n", "2.0.28"),
            ("1.18.33\n", "1.18.33"),
            ("codex-cli 0.158.0\n", "0.158.0"),
            (
                "GitHub Copilot CLI 1.0.97.\nRun 'copilot update' to check for updates.\n",
                "1.0.97",
            ),
        ] {
            assert_eq!(parse_command_version(stdout).as_deref(), Some(expected));
        }
        assert_eq!(parse_command_version("no version here 2"), None);
    }
}
