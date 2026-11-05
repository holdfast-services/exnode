//! Known-vulnerable versions of the software a provider runs beside exnode.
//!
//! **[`ADVISORIES`] is the one place to update.**  The startup and periodic
//! safety checks ([`crate::checks`]) and `exnode doctor` read nothing else.
//! Add a row when an advisory lands: a `Refuse` row stops the node taking
//! jobs while it matches, a `Warn` row only logs.  Write the affected range
//! as the advisory states it, `Below` ("prior to X") or `AtMost` ("X and
//! earlier").  A bound that doesn't parse matches every version, so a typo
//! refuses loudly instead of passing silently (and a unit test catches it).
//!
//! Versions compare numerically, part by part.  A pre-release (`0.17.1-rc0`)
//! counts as older than its release.  llama.cpp versions are build numbers
//! (`b8146`).  This module uses only `std`.

use std::cmp::Ordering;

/// What a check found, in increasing severity.  `Fail` means it could not
/// run (say, the backend is down) and `Refuse` that it found a reason never
/// to serve.  Either stops the node taking jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
    Refuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Ollama,
    LlamaCpp,
    NvidiaContainerToolkit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affected {
    /// Every version before this one ("prior to 0.17.1").
    Below(&'static str),
    /// This version and every one before it ("0.12.3 and earlier").
    AtMost(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Warn,
    Refuse,
}

#[derive(Debug)]
pub struct Advisory {
    pub component: Component,
    pub ids:       &'static str,
    pub affected:  Affected,
    pub action:    Action,
    pub summary:   &'static str,
}

use Action::{Refuse, Warn};
use Affected::{AtMost, Below};
use Component::{LlamaCpp, NvidiaContainerToolkit, Ollama};

/// The advisory table.  Keep rows grouped by component.
pub const ADVISORIES: &[Advisory] = &[
    Advisory {
        component: Ollama, ids: "CVE-2026-7482", affected: Below("0.17.1"), action: Refuse,
        summary: "GGUF loader heap over-read through /api/create, leaking process memory",
    },
    Advisory {
        component: Ollama, ids: "CVE-2025-63389", affected: AtMost("0.12.3"), action: Refuse,
        summary: "model-management API reachable without authentication",
    },
    Advisory {
        component: LlamaCpp, ids: "CVE-2025-53630, CVE-2026-27940", affected: Below("b8146"), action: Refuse,
        summary: "integer overflow in gguf_init_from_file_impl, a heap overflow from a crafted GGUF",
    },
    Advisory {
        component: LlamaCpp, ids: "CVE-2026-33298", affected: Below("b7824"), action: Refuse,
        summary: "integer overflow in ggml_nbytes, a heap overflow from a crafted GGUF",
    },
    Advisory {
        component: NvidiaContainerToolkit, ids: "CVE-2025-23266, CVE-2025-23267", affected: Below("1.17.8"),
        action: Refuse,
        summary: "container escape through the toolkit's hooks",
    },
    Advisory {
        component: NvidiaContainerToolkit, ids: "CVE-2026-24260", affected: Below("1.19.1"), action: Warn,
        summary: "TOCTOU race; NVIDIA lists 1.19.0 and earlier as affected but has not confirmed a fixed release",
    },
];

impl Component {
    pub fn name(self) -> &'static str {
        match self {
            Ollama                 => "Ollama",
            LlamaCpp               => "llama.cpp server",
            NvidiaContainerToolkit => "NVIDIA Container Toolkit",
        }
    }

    /// How to treat a version this component won't tell us.  Ollama always
    /// reports one, so silence means something else is on its port.
    /// llama.cpp hides its build in older releases and in source builds.
    pub fn if_unreadable(self) -> Level {
        match self {
            Ollama                           => Level::Refuse,
            LlamaCpp | NvidiaContainerToolkit => Level::Warn,
        }
    }

    /// A version as this component reports it.  llama.cpp's build 0 is a
    /// source build without git metadata: its real build is unknown.
    pub fn parse(self, raw: &str) -> Option<Version> {
        match self {
            LlamaCpp => Version::llamacpp_build(raw).filter(|v| v.parts != [0]),
            _        => Version::dotted(raw),
        }
    }
}

impl Affected {
    pub fn contains(self, component: Component, v: &Version) -> bool {
        let (bound, inclusive) = match self {
            Below(b)  => (b, false),
            AtMost(b) => (b, true),
        };
        match component.parse(bound) {
            Some(b) => if inclusive { *v <= b } else { *v < b },
            None    => true, // a typo in the table refuses rather than passes
        }
    }

    fn describe(self) -> String {
        match self {
            Below(b)  => format!("< {b}"),
            AtMost(b) => format!("<= {b}"),
        }
    }
}

impl Advisory {
    pub fn describe(&self) -> String {
        format!("{} ({} {}: {})", self.ids, self.component.name(), self.affected.describe(), self.summary)
    }
}

/// A version number: numeric parts and whether it is a pre-release.
#[derive(Debug, Clone)]
pub struct Version {
    parts: Vec<u64>,
    pre:   bool,
}

impl Version {
    /// `0.17.1`, `v1.17.8`, `0.17.1-rc0` (a pre-release), `1.2.3+meta`.
    pub fn dotted(raw: &str) -> Option<Self> {
        let s = raw.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let (core, pre) = match s.find(['-', '+']) {
            Some(i) => (&s[..i], s[i..].starts_with('-')),
            None    => (s, false),
        };
        let parts = core.split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<_>>>()?;
        (parts.len() <= 4).then_some(Self { parts, pre })
    }

    /// llama.cpp's build number, as in its `build_info`: `b8146-1a2b3c4d`, `b8146`.
    pub fn llamacpp_build(raw: &str) -> Option<Self> {
        let s = raw.trim();
        let s = s.strip_prefix('b').unwrap_or(s);
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (digits, rest) = s.split_at(end);
        if digits.is_empty() || !(rest.is_empty() || rest.starts_with('-')) {
            return None;
        }
        Some(Self { parts: vec![digits.parse().ok()?], pre: false })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.parts.len().max(other.parts.len());
        (0..n)
            .map(|i| self.parts.get(i).unwrap_or(&0).cmp(other.parts.get(i).unwrap_or(&0)))
            .find(|o| o.is_ne())
            // a pre-release sorts before its release
            .unwrap_or_else(|| other.pre.cmp(&self.pre))
    }
}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool { self.cmp(other) == Ordering::Equal }
}
impl Eq for Version {}

/// The advisories that affect `version` of `component`.
pub fn affecting(component: Component, version: &Version) -> Vec<&'static Advisory> {
    ADVISORIES.iter()
        .filter(|a| a.component == component && a.affected.contains(component, version))
        .collect()
}

/// The lowest version that no `Refuse` row affects: the highest bound among
/// them (an `AtMost` bound is affected itself, so it is shown as "above").
pub fn minimum(component: Component) -> Option<String> {
    ADVISORIES.iter()
        .filter(|a| a.component == component && a.action == Refuse)
        .filter_map(|a| {
            let (b, above) = match a.affected { Below(b) => (b, false), AtMost(b) => (b, true) };
            Some((component.parse(b)?, b, above))
        })
        .max_by(|x, y| x.0.cmp(&y.0).then(x.2.cmp(&y.2)))
        .map(|(_, b, above)| if above { format!("anything above {b}") } else { b.to_string() })
}

/// A reported version made safe to print: at most 40 plain characters.
pub fn clean(raw: &str) -> String {
    raw.trim().chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_' | ':'))
        .take(40)
        .collect()
}

/// The version in a `--version` banner such as
/// `NVIDIA Container Toolkit CLI version 1.17.8\ncommit: …`: the word after
/// "version", else the first word that reads as a dotted version.
pub fn version_in_banner(text: &str) -> Option<&str> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let dotted = |w: &str| w.contains('.') && Version::dotted(w).is_some();
    words.windows(2)
        .find(|p| p[0].eq_ignore_ascii_case("version") && dotted(p[1]))
        .map(|p| p[1])
        .or_else(|| words.iter().copied().find(|w| dotted(w)))
}

/// The verdict on what `component` reported as its version (`None`: it
/// reported nothing), as a level and a message for logs and `exnode doctor`.
pub fn verdict(component: Component, reported: Option<&str>) -> (Level, String) {
    let name = component.name();
    let min  = minimum(component).map(|m| format!("; the minimum to serve is {m}")).unwrap_or_default();
    let raw  = reported.map(clean).filter(|r| !r.is_empty());
    let Some(raw) = raw else {
        return (component.if_unreadable(), format!("{name} did not report its version, so it cannot be checked{min}"));
    };
    let Some(version) = component.parse(&raw) else {
        return (component.if_unreadable(), format!("cannot read {name}'s version from \"{raw}\", so it cannot be checked{min}"));
    };
    let hits = affecting(component, &version);
    let Some(worst) = hits.iter().map(|a| a.action).max() else {
        return (Level::Ok, format!("{name} {raw}: no known advisory applies{min}"));
    };
    let list = hits.iter().map(|a| a.describe()).collect::<Vec<_>>().join("; ");
    match worst {
        Refuse => (Level::Refuse, format!("{name} {raw} is affected by {list}. Upgrade it{min}")),
        Warn   => (Level::Warn, format!("{name} {raw} may be affected by {list}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version { Version::dotted(s).unwrap() }

    #[test]
    fn every_bound_in_the_table_parses() {
        for a in ADVISORIES {
            let b = match a.affected { Below(b) | AtMost(b) => b };
            assert!(a.component.parse(b).is_some(), "{}: bound {b:?} does not parse", a.ids);
        }
    }

    #[test]
    fn versions_compare_numerically_and_pre_releases_come_first() {
        assert!(v("0.9.0") < v("0.17.1"));
        assert!(v("0.17.0") < v("0.17.1"));
        assert!(v("0.17") < v("0.17.1"));
        assert_eq!(v("1.17.8"), v("v1.17.8"));
        assert_eq!(v("1.2"), v("1.2.0"));
        assert!(v("0.17.1-rc0") < v("0.17.1"));
        assert!(v("0.17.1-rc0") > v("0.17.0"));
        assert_eq!(v("0.17.1+build7"), v("0.17.1"));
        for bad in ["", "abc", "1..2", "1.2.x", "1.2.3.4.5"] {
            assert!(Version::dotted(bad).is_none(), "{bad}");
        }
        assert!(Version::llamacpp_build("b8146-1a2b3c4d").unwrap() == Version::llamacpp_build("8146").unwrap());
        assert!(Version::llamacpp_build("b7823").unwrap() < Version::llamacpp_build("b8146").unwrap());
        assert!(Version::llamacpp_build("bogus").is_none());
        assert!(Version::llamacpp_build("b81x46").is_none());
        assert!(LlamaCpp.parse("b0-unknown").is_none(), "build 0 is a source build of unknown age");
    }

    #[test]
    fn ollama_needs_0_17_1() {
        assert_eq!(minimum(Ollama).as_deref(), Some("0.17.1"));
        assert_eq!(verdict(Ollama, Some("0.17.1")).0, Level::Ok);
        assert_eq!(verdict(Ollama, Some("0.18.0")).0, Level::Ok);
        let (level, msg) = verdict(Ollama, Some("0.17.0"));
        assert_eq!(level, Level::Refuse);
        assert!(msg.contains("CVE-2026-7482") && !msg.contains("CVE-2025-63389"), "{msg}");
        let (level, msg) = verdict(Ollama, Some("0.12.3"));
        assert_eq!(level, Level::Refuse);
        assert!(msg.contains("CVE-2026-7482") && msg.contains("CVE-2025-63389"), "{msg}");
        assert!(!verdict(Ollama, Some("0.12.4")).1.contains("CVE-2025-63389"));
        assert_eq!(verdict(Ollama, Some("0.17.1-rc2")).0, Level::Refuse, "a pre-release is not the fix");
        assert_eq!(verdict(Ollama, Some("0.0.0")).0, Level::Refuse, "a dev build is not a release");
        assert_eq!(verdict(Ollama, None).0, Level::Refuse);
        assert_eq!(verdict(Ollama, Some("garbage")).0, Level::Refuse);
    }

    #[test]
    fn llama_cpp_needs_b8146_and_an_unknown_build_only_warns() {
        assert_eq!(minimum(LlamaCpp).as_deref(), Some("b8146"));
        assert_eq!(verdict(LlamaCpp, Some("b8146-abcdef0")).0, Level::Ok);
        let (level, msg) = verdict(LlamaCpp, Some("b8000-abcdef0"));
        assert_eq!(level, Level::Refuse);
        assert!(msg.contains("CVE-2026-27940") && !msg.contains("CVE-2026-33298"), "{msg}");
        let (_, msg) = verdict(LlamaCpp, Some("b7000"));
        assert!(msg.contains("CVE-2026-33298"), "{msg}");
        assert_eq!(verdict(LlamaCpp, None).0, Level::Warn);
        assert_eq!(verdict(LlamaCpp, Some("b0-unknown")).0, Level::Warn);
    }

    #[test]
    fn nvidia_toolkit_refuses_below_1_17_8_and_warns_below_1_19_1() {
        let banner = "NVIDIA Container Toolkit CLI version 1.17.7\ncommit: 0123456789abcdef";
        assert_eq!(version_in_banner(banner), Some("1.17.7"));
        assert_eq!(version_in_banner("nvidia-ctk 1.19.1 (built 2026)"), Some("1.19.1"));
        assert_eq!(version_in_banner("no version here"), None);
        assert_eq!(verdict(NvidiaContainerToolkit, version_in_banner(banner)).0, Level::Refuse);
        assert_eq!(verdict(NvidiaContainerToolkit, Some("1.17.8")).0, Level::Warn);
        assert_eq!(verdict(NvidiaContainerToolkit, Some("1.19.0")).0, Level::Warn);
        assert_eq!(verdict(NvidiaContainerToolkit, Some("1.19.1")).0, Level::Ok);
        assert_eq!(verdict(NvidiaContainerToolkit, None).0, Level::Warn);
    }

    #[test]
    fn reported_versions_are_cleaned_before_printing() {
        assert_eq!(clean(" 0.17.1\n\u{1b}[31m; rm -rf /"), "0.17.131mrm-rf");
        assert_eq!(clean(&"9".repeat(100)).len(), 40);
    }
}
