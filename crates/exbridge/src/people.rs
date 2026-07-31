//! Contributor enrichment via kraken.
//!
//! entropyx knows *emails*. kraken knows *people* — real names, employers,
//! org membership, working hours, career history — and the two join on the
//! email address in `Summary.dict.authors`.
//!
//! The coverage caveat is load-bearing and is surfaced, not buried.
//! kraken crawls GitHub's identity graph; entropyx reads local commit
//! history. They only overlap where a contributor's public GitHub activity
//! exposes the same address they commit with. Measured on a 101-author
//! repository, a repo-seeded crawl resolved **1 contributor — but that one
//! wrote 68% of the commits**. Coverage by headcount and coverage by
//! contribution are wildly different numbers, so this module reports both
//! and never lets a caller quote one alone.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::Command;

/// Cap the crawl. kraken spends GitHub GraphQL budget (5,000 points/hour,
/// roughly one per user), and an unbounded spider on a large org would
/// burn it for a sidebar.
const MAX_USERS: &str = "60";
const MAX_REPOS: &str = "8";

// ---- kraken report subset -------------------------------------------

#[derive(Debug, Deserialize)]
struct KrakenReport {
    #[serde(default)]
    persons: Vec<KrakenPerson>,
    #[serde(default)]
    stats: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct KrakenPerson {
    #[serde(default)]
    name: String,
    #[serde(default)]
    github_logins: Vec<String>,
    #[serde(default)]
    emails: Vec<KrakenEmail>,
    #[serde(default)]
    orgs: Vec<String>,
    #[serde(default)]
    total_commits: u64,
    #[serde(default)]
    profile: Option<KrakenProfile>,
    #[serde(default)]
    timezone: Option<KrakenTimezone>,
    #[serde(default)]
    work_pattern: Option<KrakenWorkPattern>,
    #[serde(default)]
    career: Vec<KrakenCareerStep>,
}

#[derive(Debug, Deserialize)]
struct KrakenEmail {
    email: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    domain_kind: String,
}

#[derive(Debug, Deserialize)]
struct KrakenProfile {
    #[serde(default)]
    company: Option<String>,
    #[serde(default)]
    location: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KrakenTimezone {
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    region: String,
}

#[derive(Debug, Deserialize)]
struct KrakenWorkPattern {
    #[serde(default)]
    pattern: String,
}

#[derive(Debug, Deserialize)]
struct KrakenCareerStep {
    #[serde(default)]
    domain: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    first_seen: String,
    #[serde(default)]
    last_seen: String,
    #[serde(default)]
    commits: u64,
}

// ---- what the bridge exposes ----------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct Person {
    pub name: String,
    pub logins: Vec<String>,
    /// Every address this person commits under that kraken knows about.
    pub emails: Vec<String>,
    /// Employer inferred from a corporate email domain, if any. Personal
    /// providers (gmail, proton, …) are not employers and are excluded.
    pub employer: Option<String>,
    pub company_profile: Option<String>,
    pub location: Option<String>,
    pub orgs: Vec<String>,
    /// kraken's count across all of GitHub, **not** this repository.
    /// Callers must label it as such or not show it.
    pub total_commits: u64,
    /// Inferred from a commit-hour histogram, and carried here only so a
    /// caller has the option. It is **not rendered by the sheet or the
    /// brief**: measured across 37 contributors the median confidence was
    /// 0.00, and it placed a verifiably Vienna-based developer in
    /// "India / Central Asia" at 0.48. A geographic claim that wrong is
    /// worse than no claim, so the UI omits it.
    pub timezone: Option<String>,
    pub timezone_confidence: Option<f64>,
    pub work_pattern: Option<String>,
    pub career: Vec<CareerStep>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CareerStep {
    pub domain: String,
    pub kind: String,
    pub first_seen: String,
    pub last_seen: String,
    pub commits: u64,
}

#[derive(Serialize)]
pub struct Coverage {
    /// Distinct author addresses in the local history.
    pub authors: usize,
    /// How many of those kraken could put a person behind.
    pub resolved: usize,
    /// Share of local commits written by resolved authors. This is almost
    /// always far higher than `resolved / authors`, because the people a
    /// public crawl finds are the prolific ones.
    pub commit_share: f64,
    /// Addresses that can never resolve — GitHub noreply and bot
    /// identities, which kraken filters by design.
    pub unresolvable: usize,
}

#[derive(Serialize)]
pub struct PeopleReport {
    pub available: bool,
    /// Why enrichment is unavailable, when it is. Never silent.
    pub reason: Option<String>,
    pub seed: Option<String>,
    pub persons: Vec<Person>,
    pub by_email: BTreeMap<String, usize>,
    pub coverage: Coverage,
    pub stats: serde_json::Value,
}

fn unavailable(reason: &str) -> PeopleReport {
    PeopleReport {
        available: false,
        reason: Some(reason.to_string()),
        seed: None,
        persons: Vec::new(),
        by_email: BTreeMap::new(),
        coverage: Coverage {
            authors: 0,
            resolved: 0,
            commit_share: 0.0,
            unresolvable: 0,
        },
        stats: serde_json::json!({}),
    }
}

/// Free-provider domains. A gmail address tells you nothing about who
/// someone works for, and presenting it as an employer would be a lie
/// dressed as a data point.
const PERSONAL_DOMAINS: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "yahoo.com",
    "protonmail.com",
    "proton.me",
    "icloud.com",
    "me.com",
    "qq.com",
    "163.com",
    "126.com",
    "naver.com",
    "yandex.ru",
    "gmx.de",
    "web.de",
    "fastmail.com",
    "hey.com",
    "pm.me",
    "duck.com",
    "users.noreply.github.com",
];

fn is_employer_domain(domain: &str, kind: &str) -> bool {
    kind == "corporate"
        && !PERSONAL_DOMAINS
            .iter()
            .any(|d| domain.eq_ignore_ascii_case(d))
}

/// An address that can never be resolved to a person: GitHub's noreply
/// relay, or an automation identity.
pub fn is_unresolvable(email: &str) -> bool {
    let e = email.to_ascii_lowercase();
    e.contains("noreply")
        || e.contains("no-reply")
        || e.ends_with("[bot]")
        || e.contains("actions@github.com")
}

/// Per-author commit counts from the local history. kraken's own commit
/// numbers count that person's activity across all of GitHub, which is a
/// different question from "how much of *this* repository did they write".
fn local_commit_counts(repo: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let Ok(o) = Command::new("git")
        .args(["log", "--format=%ae"])
        .current_dir(repo)
        .output()
    else {
        return out;
    };
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let e = line.trim().to_ascii_lowercase();
        if !e.is_empty() {
            *out.entry(e).or_insert(0) += 1;
        }
    }
    out
}

/// Resolve a GitHub token: the environment first, then the `gh` CLI's
/// stored credential, which is how this machine is usually authenticated.
fn github_token() -> Option<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN")
        && !t.trim().is_empty()
    {
        return Some(t);
    }
    let out = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Infer `owner/name` from the repository's origin remote.
pub fn github_slug(repo: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_slug(String::from_utf8_lossy(&out.stdout).trim())
}

pub fn parse_slug(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/');
    let rest = [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
    ]
    .iter()
    .find_map(|p| u.strip_prefix(p))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    (!owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
}

/// Enrich a repository's contributors.
///
/// `authors` are the addresses entropyx found in the local history —
/// passed in so coverage is measured against what the survey actually
/// shows, not against whatever kraken happens to return.
pub fn enrich(repo: &str, authors: &[String], seed_override: Option<&str>) -> PeopleReport {
    let seed = match seed_override {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => match github_slug(repo) {
            Some(s) => s,
            None => {
                return unavailable(
                    "this repository is not on GitHub, so there are no public profiles to look names up in",
                );
            }
        },
    };

    let Some(token) = github_token() else {
        return unavailable(
            "no GitHub token available. Set GITHUB_TOKEN, or run `gh auth login` and try again",
        );
    };

    let out = Command::new("kraken")
        .args([
            &seed,
            "-d",
            "0",
            "--max-users",
            MAX_USERS,
            "--max-repos",
            MAX_REPOS,
            "-f",
            "json",
        ])
        .env("GITHUB_TOKEN", token)
        .output();

    let out = match out {
        Ok(o) => o,
        Err(e) => return unavailable(&format!("kraken is not runnable: {e}")),
    };
    if !out.status.success() {
        return unavailable(&format!(
            "kraken failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let report: KrakenReport = match serde_json::from_slice(&out.stdout) {
        Ok(r) => r,
        Err(e) => return unavailable(&format!("kraken output unreadable: {e}")),
    };

    // Distinguish "the crawl found nobody matching" from "the crawl never
    // reached the repository". A private, renamed or non-existent slug
    // returns a clean empty report, and reporting that as 0% coverage
    // would blame the data for a permissions problem.
    let repos_scanned = report
        .stats
        .get("repos_scanned")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if report.persons.is_empty() && repos_scanned == 0 {
        // Note the ambiguity rather than asserting a cause: a private
        // repo, a renamed one, and a transient API failure all produce
        // this same empty report. A transient failure was observed once
        // during development and succeeded on retry.
        return unavailable(&format!(
            "GitHub returned nothing for `{seed}`. It may be private, renamed, or invisible to \
             this token — or the request may simply have failed this time. Worth retrying."
        ));
    }

    let mut persons = Vec::new();
    let mut by_email: BTreeMap<String, usize> = BTreeMap::new();

    for p in report.persons {
        let idx = persons.len();
        let employer = p
            .emails
            .iter()
            .find(|e| is_employer_domain(&e.domain, &e.domain_kind))
            .map(|e| e.domain.clone());

        let emails: Vec<String> = p
            .emails
            .iter()
            .map(|e| e.email.to_ascii_lowercase())
            .collect();
        for e in &emails {
            by_email.insert(e.clone(), idx);
        }

        persons.push(Person {
            name: if p.name.is_empty() {
                p.github_logins.first().cloned().unwrap_or_default()
            } else {
                p.name
            },
            logins: p.github_logins,
            emails,
            employer,
            company_profile: p.profile.as_ref().and_then(|x| x.company.clone()),
            location: p.profile.as_ref().and_then(|x| x.location.clone()),
            orgs: p.orgs,
            total_commits: p.total_commits,
            timezone: p.timezone.as_ref().map(|t| t.region.clone()),
            timezone_confidence: p.timezone.as_ref().map(|t| t.confidence),
            work_pattern: p.work_pattern.as_ref().map(|w| w.pattern.clone()),
            career: p
                .career
                .into_iter()
                .map(|c| CareerStep {
                    domain: c.domain,
                    kind: c.kind,
                    first_seen: c.first_seen,
                    last_seen: c.last_seen,
                    commits: c.commits,
                })
                .collect(),
        });
    }

    let counts = local_commit_counts(repo);
    let total: u64 = counts.values().sum();
    let mut resolved = 0usize;
    let mut resolved_commits = 0u64;
    let mut unresolvable = 0usize;
    for a in authors {
        let key = a.to_ascii_lowercase();
        if is_unresolvable(&key) {
            unresolvable += 1;
        }
        if by_email.contains_key(&key) {
            resolved += 1;
            resolved_commits += counts.get(&key).copied().unwrap_or(0);
        }
    }

    PeopleReport {
        available: true,
        reason: None,
        seed: Some(seed),
        persons,
        by_email,
        coverage: Coverage {
            authors: authors.len(),
            resolved,
            commit_share: if total > 0 {
                resolved_commits as f64 / total as f64
            } else {
                0.0
            },
            unresolvable,
        },
        stats: report.stats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_github_remote_form() {
        for u in [
            "git@github.com:copyleftdev/entropyx.git",
            "https://github.com/copyleftdev/entropyx.git",
            "https://github.com/copyleftdev/entropyx",
            "http://github.com/copyleftdev/entropyx",
            "ssh://git@github.com/copyleftdev/entropyx.git",
        ] {
            assert_eq!(
                parse_slug(u).as_deref(),
                Some("copyleftdev/entropyx"),
                "{u}"
            );
        }
    }

    #[test]
    fn rejects_non_github_remotes() {
        assert_eq!(parse_slug("git@gitlab.com:a/b.git"), None);
        assert_eq!(parse_slug("/srv/git/local.git"), None);
        assert_eq!(parse_slug(""), None);
    }

    #[test]
    fn personal_providers_are_never_employers() {
        // A gmail address is corporate-shaped to a naive classifier but
        // says nothing about who anyone works for.
        assert!(!is_employer_domain("gmail.com", "corporate"));
        assert!(!is_employer_domain("users.noreply.github.com", "corporate"));
        assert!(is_employer_domain("codetestcode.io", "corporate"));
    }

    #[test]
    fn non_corporate_kinds_are_never_employers() {
        assert!(!is_employer_domain("mit.edu", "academic"));
        assert!(!is_employer_domain("kernel.org", "open_source"));
    }

    #[test]
    fn noreply_and_bot_addresses_are_unresolvable() {
        assert!(is_unresolvable("12345678+someone@users.noreply.github.com"));
        assert!(is_unresolvable("actions@github.com"));
        assert!(is_unresolvable("dependabot[bot]"));
        assert!(!is_unresolvable("someone@example.com"));
    }
}
