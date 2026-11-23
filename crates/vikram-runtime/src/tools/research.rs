use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::config::Config;
use crate::search::SearchResult;
use crate::tools::*;

/// One question becomes a few queries, never a crawl: three variants, five distinct pages.
const MAX_QUERIES: usize = 3;
const MAX_SOURCES: usize = 5;
const RESULTS_PER_QUERY: usize = 5;
/// Characters requested per page. `browser.fetch` has its own 20 s request cap, as does the
/// search client; this budget is the stop the whole call obeys, checked between requests.
const PAGE_CHARS: usize = 1_500;
const EXCERPT_CHARS: usize = 700;
const BUDGET: Duration = Duration::from_secs(45);

pub(crate) struct Research;

impl Tool for Research {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "research".into(),
            description: "Answer one question from the web: run a few searches, fetch the top \
                          distinct pages, and return one digest with every source named."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to research; it is also the primary search query."
                    },
                    "max_sources": {
                        "type": "integer",
                        "description": "Pages to read at most, capped at 5."
                    }
                },
                "required": ["question"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("question")
            .to_owned();
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "research",
            question,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let question = arg_str(args, "question")?;
        let wanted = args
            .get("max_sources")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .filter(|value| *value > 0)
            .unwrap_or(MAX_SOURCES)
            .min(MAX_SOURCES);

        let config = match Config::load() {
            Ok(config) => config,
            Err(error) => {
                return Ok(ToolOutput {
                    content: format!("research is not configured: {error}"),
                    ok: false,
                });
            }
        };
        let api_key = config.search_api_key_with_source().map(|(value, _)| value);
        let client = match crate::search::client() {
            Ok(client) => client,
            Err(error) => {
                return Ok(ToolOutput {
                    content: format!("research cannot start: {error}"),
                    ok: false,
                });
            }
        };

        let mut search = |query: &str| {
            crate::search::search(
                &config.search,
                api_key.as_deref(),
                &client,
                query,
                RESULTS_PER_QUERY,
            )
            .map_err(|error| error.to_string())
        };
        let mut fetch = |url: &str| {
            let call = BrowserFetch
                .run(&json!({"url": url, "max_length": PAGE_CHARS}), ctx)
                .map_err(|error| error.to_string())?;
            if call.ok {
                Ok(call.content)
            } else {
                Err(call.content)
            }
        };

        let findings = gather(
            &question,
            Instant::now(),
            wanted,
            ctx.cancel.as_ref(),
            &mut search,
            &mut fetch,
        );
        let content = digest(&question, &findings);
        Ok(ToolOutput {
            content: truncated(content),
            ok: !findings.pages.is_empty(),
        })
    }
}

struct Findings {
    urls: Vec<String>,
    reached: Vec<usize>,
    pages: Vec<(String, String)>,
    fetch_failures: Vec<String>,
    search_failures: Vec<String>,
    stopped: Option<&'static str>,
}

/// Search the variants, keep the top distinct URLs, then read each page once. Every network call
/// comes from the caller's closures, which is how the real backends stay the only HTTP in play and
/// how a test can drive the pipeline without a socket.
fn gather(
    question: &str,
    started: Instant,
    wanted: usize,
    cancel: Option<&vikram_contracts::CancellationToken>,
    search: &mut dyn FnMut(&str) -> Result<Vec<SearchResult>, String>,
    fetch: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Findings {
    let mut findings = Findings {
        urls: Vec::new(),
        reached: Vec::new(),
        pages: Vec::new(),
        fetch_failures: Vec::new(),
        search_failures: Vec::new(),
        stopped: None,
    };
    let mut key_index: HashMap<String, usize> = HashMap::new();

    for query in query_variants(question) {
        if stopped(started, cancel) {
            findings.stopped = Some(stop_reason(started, cancel));
            break;
        }
        match search(&query) {
            Ok(results) => {
                let mut reached_now: HashSet<String> = HashSet::new();
                for result in results {
                    let Some(key) = url_key(&result.url) else {
                        continue;
                    };
                    match key_index.get(&key).copied() {
                        Some(index) => {
                            if reached_now.insert(key) {
                                findings.reached[index] += 1;
                            }
                        }
                        None => {
                            if findings.urls.len() >= wanted {
                                continue;
                            }
                            key_index.insert(key.clone(), findings.urls.len());
                            reached_now.insert(key);
                            findings.urls.push(result.url);
                            findings.reached.push(1);
                        }
                    }
                }
            }
            Err(error) => findings.search_failures.push(format!("`{query}`: {error}")),
        }
    }

    for url in &findings.urls {
        if stopped(started, cancel) {
            findings.stopped = Some(stop_reason(started, cancel));
            break;
        }
        match fetch(url) {
            Ok(page) => findings.pages.push((url.clone(), page)),
            Err(error) => findings.fetch_failures.push(format!("{url}: {error}")),
        }
    }
    findings
}

fn stopped(started: Instant, cancel: Option<&vikram_contracts::CancellationToken>) -> bool {
    started.elapsed() >= BUDGET || cancel.is_some_and(|token| token.is_cancelled())
}

fn stop_reason(
    started: Instant,
    cancel: Option<&vikram_contracts::CancellationToken>,
) -> &'static str {
    if started.elapsed() >= BUDGET {
        "the wall-clock budget ran out"
    } else if cancel.is_some_and(|token| token.is_cancelled()) {
        "the turn was cancelled"
    } else {
        "the call stopped early"
    }
}

fn query_variants(question: &str) -> Vec<String> {
    let question = question.trim();
    let mut variants = vec![question.to_owned()];
    for suffix in [" explained", " recent developments"] {
        if variants.len() >= MAX_QUERIES {
            break;
        }
        variants.push(format!("{question}{suffix}"));
    }
    variants
}

/// Host plus path, with the scheme, `www`, query and fragment removed: two search hits for one page
/// are one page, whatever campaign parameter each URL carries.
fn url_key(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let rest = rest.split('#').next()?;
    let rest = rest.split('?').next()?;
    let (host, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, ""),
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host.is_empty() {
        return None;
    }
    Some(format!("{host}{}", path.trim_end_matches('/')))
}

const RANK_STOPWORDS: &[&str] = &[
    "the", "and", "are", "but", "can", "did", "does", "for", "from", "has", "have", "how", "into",
    "its", "not", "that", "this", "was", "were", "what", "when", "where", "which", "who", "why",
    "with", "you", "your",
];

/// The question's own words, lowercased and de-duplicated. This is a lexical overlap check, not a
/// stemmer: it says whether the page's opening repeats what was asked, nothing about relevance.
fn query_terms(question: &str) -> Vec<String> {
    let mut terms: Vec<String> = question
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| word.chars().count() >= 3)
        .map(|word| word.to_ascii_lowercase())
        .filter(|word| !RANK_STOPWORDS.contains(&word.as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    terms.sort();
    terms
}

fn page_head(page: &str) -> String {
    let title = page
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Title: "))
        .unwrap_or("");
    let body = page
        .split_once("\n\n")
        .map(|(_, body)| body)
        .unwrap_or(page);
    let paragraph = body.split("\n\n").next().unwrap_or(body);
    format!("{title} {}", one_line(paragraph, EXCERPT_CHARS)).to_ascii_lowercase()
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(needle).any(|(start, _)| {
        let before = haystack[..start].chars().next_back();
        let after = haystack[start + needle.len()..].chars().next();
        before.is_none_or(|ch| !ch.is_alphanumeric())
            && after.is_none_or(|ch| !ch.is_alphanumeric())
    })
}

fn term_hits(terms: &[String], page: &str) -> usize {
    let head = page_head(page);
    terms
        .iter()
        .filter(|term| contains_word(&head, term))
        .count()
}

fn reached_count(findings: &Findings, url: &str) -> usize {
    let Some(key) = url_key(url) else {
        return 0;
    };
    findings
        .urls
        .iter()
        .position(|candidate| url_key(candidate).as_deref() == Some(key.as_str()))
        .and_then(|index| findings.reached.get(index).copied())
        .unwrap_or(0)
}

/// Most queries first, then most query words in the page's opening, then fetch order. Every term of
/// the sort is a fact about the search, so the same input always yields the same ranking.
fn rank(question: &str, findings: &Findings) -> Vec<usize> {
    let terms = query_terms(question);
    let scores: Vec<(usize, usize)> = findings
        .pages
        .iter()
        .map(|(url, page)| (reached_count(findings, url), term_hits(&terms, page)))
        .collect();
    let mut order: Vec<usize> = (0..findings.pages.len()).collect();
    order.sort_by_key(|index| (Reverse(scores[*index].0), Reverse(scores[*index].1), *index));
    order
}

const NEGATIVE_MARKERS: &[&str] = &[
    "is not supported",
    "not supported",
    "no longer supported",
    "no longer",
    "does not work",
    "doesn't work",
    "does not support",
    "doesn't support",
    "is not available",
    "not available",
    "is not possible",
    "is not recommended",
    "fails to work",
    "is removed",
    "unsupported",
    "deprecated",
];

const POSITIVE_MARKERS: &[&str] = &[
    "is supported",
    "supported",
    "works",
    "work",
    "is available",
    "is possible",
    "is recommended",
    "succeeds",
    "functions",
    "can be used",
    "is working",
];

fn has_marker(sentence: &str, marker: &str) -> bool {
    if marker.contains([' ', '\'']) {
        sentence.contains(marker)
    } else {
        contains_word(sentence, marker)
    }
}

/// Polarity for one subject, read only from sentences that name the subject. Negatives are matched
/// first so "is not supported" cannot be counted as support, and a page that says both is treated
/// as no signal rather than a conflict.
fn polarity(page: &str, subject: &str) -> Option<bool> {
    let lower = page.to_ascii_lowercase();
    let mut positive = false;
    let mut negative = false;
    for sentence in lower.split(['.', '!', '?', '\n']) {
        if !contains_word(sentence, subject) {
            continue;
        }
        if NEGATIVE_MARKERS
            .iter()
            .any(|marker| has_marker(sentence, marker))
        {
            negative = true;
        } else if POSITIVE_MARKERS
            .iter()
            .any(|marker| has_marker(sentence, marker))
        {
            positive = true;
        }
    }
    match (positive, negative) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// One line per pair that takes opposite sides on a shared query word. It reports the marker clash,
/// not a settled contradiction: the reader has to check whether the two pages are even answering
/// the same question.
fn disagreements(question: &str, findings: &Findings) -> Vec<(String, String, String)> {
    let terms = query_terms(question);
    let mut conflicts = Vec::new();
    for left in 0..findings.pages.len() {
        for right in (left + 1)..findings.pages.len() {
            let (left_url, left_page) = &findings.pages[left];
            let (right_url, right_page) = &findings.pages[right];
            let subjects: Vec<String> = terms
                .iter()
                .filter(|term| {
                    matches!(
                        (polarity(left_page, term), polarity(right_page, term)),
                        (Some(left_polarity), Some(right_polarity)) if left_polarity != right_polarity
                    )
                })
                .map(|term| format!("`{term}`"))
                .collect();
            if !subjects.is_empty() {
                conflicts.push((subjects.join(", "), left_url.clone(), right_url.clone()));
            }
        }
    }
    conflicts
}

fn digest(question: &str, findings: &Findings) -> String {
    let mut out = format!("Research: \"{question}\"\n\n");
    let attempted = findings.pages.len() + findings.fetch_failures.len();

    if findings.pages.is_empty() {
        if findings.urls.is_empty() {
            let cause = match findings.search_failures.len() {
                0 => "the searches ran and returned nothing usable".to_owned(),
                1 => "the search failed".to_owned(),
                failed => format!("{failed} searches failed"),
            };
            out.push_str(&format!(
                "No sources: {cause}, so there is no finding to report. This is not an answer to \
                 the question.\n"
            ));
        } else {
            out.push_str(&format!(
                "No finding: {} {} found but every page fetch failed, so nothing was read. \
                 This is not an answer to the question.\n",
                findings.urls.len(),
                if findings.urls.len() == 1 {
                    "source was"
                } else {
                    "sources were"
                }
            ));
        }
        out.push('\n');
    }

    let order = rank(question, findings);
    let total = order.len();
    for (position, index) in order.iter().enumerate() {
        let (url, page) = &findings.pages[*index];
        out.push_str(&format!(
            "[{}/{}] {url}\n  what I found: {}\n\n",
            position + 1,
            total,
            excerpt(page)
        ));
    }

    let conflicts = disagreements(question, findings);
    if !conflicts.is_empty() {
        out.push_str("possible disagreement:\n");
        for (subjects, left, right) in &conflicts {
            let line = one_line(&format!("about {subjects}: {left} vs {right}"), 300);
            out.push_str(&format!("  {line}\n"));
        }
        out.push('\n');
    }

    if !findings.fetch_failures.is_empty() {
        out.push_str("Failed pages:\n");
        for failure in &findings.fetch_failures {
            out.push_str(&format!("  {}\n", one_line(failure, 300)));
        }
        out.push('\n');
    }
    if !findings.search_failures.is_empty() {
        out.push_str("Failed searches:\n");
        for failure in &findings.search_failures {
            out.push_str(&format!("  {}\n", one_line(failure, 300)));
        }
        out.push('\n');
    }
    if let Some(reason) = findings.stopped {
        out.push_str(&format!("Stopped early: {reason}.\n\n"));
    }
    out.push_str(&format!(
        "sources read: {}; failed: {} (of {attempted} attempted)\n",
        findings.pages.len(),
        findings.fetch_failures.len()
    ));
    out
}

/// The page's own opening — title plus the first sentences `browser.fetch` returned. It is quoted
/// text, not a summary the tool wrote, which is why it is labelled "what I found" on the URL.
fn excerpt(page: &str) -> String {
    let title = page
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Title: "))
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let body = page
        .split_once("\n\n")
        .map(|(_, body)| body)
        .unwrap_or(page);
    let body = one_line(body, EXCERPT_CHARS);
    match title {
        Some(title) => format!("{title} — {body}"),
        None => body,
    }
}

fn one_line(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let head: String = collapsed.chars().take(limit).collect();
    let cut = head
        .rfind(['.', '!', '?'])
        .filter(|index| *index >= limit / 2)
        .map(|index| &head[..=index])
        .unwrap_or(&head);
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(url: &str) -> SearchResult {
        SearchResult {
            title: "A title".to_owned(),
            url: url.to_owned(),
            snippet: "A snippet".to_owned(),
        }
    }

    #[test]
    fn a_search_that_finds_nothing_is_reported_as_no_sources() {
        let mut search = |_: &str| Ok(Vec::new());
        let mut fetch = |_: &str| Err("nothing should be fetched".to_owned());
        let findings = gather(
            "why is the sky blue",
            Instant::now(),
            5,
            None,
            &mut search,
            &mut fetch,
        );
        assert!(findings.urls.is_empty());
        let text = digest("why is the sky blue", &findings);
        assert!(text.contains("No sources"), "{text}");
        assert!(!text.contains("what I found:"), "{text}");
        assert!(
            text.ends_with("sources read: 0; failed: 0 (of 0 attempted)\n"),
            "{text}"
        );
    }

    #[test]
    fn one_page_reached_by_two_queries_is_fetched_once() {
        let mut search = |_: &str| {
            Ok(vec![
                result("https://www.example.com/a/?utm_source=x"),
                result("https://example.com/b#top"),
            ])
        };
        let mut fetch = |url: &str| Ok(format!("Title: T\nURL: {url}\n\nbody text"));
        let findings = gather("q", Instant::now(), 5, None, &mut search, &mut fetch);
        assert_eq!(
            findings.urls,
            vec![
                "https://www.example.com/a/?utm_source=x",
                "https://example.com/b#top"
            ]
        );
        assert_eq!(
            findings.pages.len(),
            2,
            "two queries must not double the reads"
        );
    }

    #[test]
    fn every_page_failing_is_said_out_loud() {
        let mut search = |_: &str| Ok(vec![result("https://example.com/a")]);
        let mut fetch = |_: &str| Err("http error: 403 Forbidden".to_owned());
        let findings = gather("q", Instant::now(), 5, None, &mut search, &mut fetch);
        let text = digest("q", &findings);
        assert!(text.contains("every page fetch failed"), "{text}");
        assert!(text.contains("403 Forbidden"), "{text}");
        assert!(text.contains("sources read: 0; failed: 1"), "{text}");
    }

    #[test]
    fn a_failing_search_is_named_and_does_not_panic() {
        let mut search = |query: &str| Err(format!("{query} was rejected"));
        let mut fetch = |_: &str| Err("nothing should be fetched".to_owned());
        let findings = gather("q", Instant::now(), 5, None, &mut search, &mut fetch);
        let text = digest("q", &findings);
        assert!(text.contains("Failed searches:"), "{text}");
        assert!(text.contains("was rejected"), "{text}");
    }

    #[test]
    fn a_source_reached_by_two_queries_ranks_above_one_reached_by_one() {
        let mut search = |query: &str| match query {
            "q" | "q explained" => Ok(vec![result("https://example.com/twice")]),
            _ => Ok(vec![result("https://example.com/once")]),
        };
        let mut fetch = |url: &str| Ok(format!("Title: T\nURL: {url}\n\nbody text"));
        let findings = gather("q", Instant::now(), 5, None, &mut search, &mut fetch);
        assert_eq!(findings.reached, vec![2, 1]);
        let text = digest("q", &findings);
        let twice = text
            .find("[1/2] https://example.com/twice")
            .expect("the twice-reached page is ranked first");
        let once = text
            .find("[2/2] https://example.com/once")
            .expect("the once-reached page is ranked second");
        assert!(twice < once, "{text}");
    }

    #[test]
    fn two_sources_that_disagree_are_reported_as_a_conflict() {
        let mut search = |_: &str| {
            Ok(vec![
                result("https://example.com/no"),
                result("https://example.com/yes"),
            ])
        };
        let mut fetch = |url: &str| {
            let body = if url.ends_with("/no") {
                "Rust async is not supported on this platform."
            } else {
                "Rust async is supported on this platform."
            };
            Ok(format!("Title: Rust async\nURL: {url}\n\n{body}"))
        };
        let findings = gather(
            "is rust async supported",
            Instant::now(),
            5,
            None,
            &mut search,
            &mut fetch,
        );
        let text = digest("is rust async supported", &findings);
        assert!(text.contains("possible disagreement:"), "{text}");
        let section = &text[text
            .find("possible disagreement:")
            .expect("conflict section is present")..];
        assert!(section.contains("https://example.com/no"), "{text}");
        assert!(section.contains("https://example.com/yes"), "{text}");
    }

    #[test]
    fn sources_that_agree_produce_no_conflict_line() {
        let mut search = |_: &str| {
            Ok(vec![
                result("https://example.com/one"),
                result("https://example.com/two"),
            ])
        };
        let mut fetch = |url: &str| {
            Ok(format!(
                "Title: Rust async\nURL: {url}\n\nRust async is supported on this platform."
            ))
        };
        let findings = gather(
            "is rust async supported",
            Instant::now(),
            5,
            None,
            &mut search,
            &mut fetch,
        );
        let text = digest("is rust async supported", &findings);
        assert!(!text.contains("possible disagreement:"), "{text}");
    }

    #[test]
    fn url_keys_drop_scheme_host_prefix_query_and_fragment() {
        assert_eq!(
            url_key("https://WWW.Example.com/a/b/?x=1#frag").as_deref(),
            Some("example.com/a/b")
        );
        assert_eq!(
            url_key("http://example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            url_key("https://example.com/").as_deref(),
            url_key("https://example.com").as_deref()
        );
        assert_eq!(url_key("ftp://example.com/a"), None);
        assert_eq!(url_key("not a url"), None);
    }

    #[test]
    fn the_query_variants_stay_inside_the_bound() {
        let variants = query_variants("  who won  ");
        assert_eq!(variants.len(), MAX_QUERIES);
        assert_eq!(variants[0], "who won");
        assert!(variants[1].starts_with("who won"));
    }

    #[test]
    fn the_excerpt_quotes_the_page_and_names_the_url_source() {
        let page = "Title: Rust 1.89\nURL: https://example.com/rust\n\nRust 1.89 is out. It ships \
                    several changes. More details follow.";
        assert_eq!(
            excerpt(page),
            "Rust 1.89 — Rust 1.89 is out. It ships several changes. More details follow."
        );
    }

    #[test]
    fn the_tool_is_registered_and_advertises_its_schema() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry.get("research").expect("research registered");
        assert_eq!(tool.spec().name, "research");
        assert_eq!(tool.spec().schema["type"], "object");
        assert_eq!(
            tool.spec().schema["properties"]["question"]["type"],
            "string"
        );
        assert_eq!(tool.spec().schema["required"][0], "question");
        assert!(registry.names().contains(&"research".to_owned()));
    }

    #[test]
    fn the_permission_is_a_network_read() {
        let request = Research.permission(&json!({"question": "q"})).unwrap();
        assert_eq!(request.action, ActionKind::Network);
        assert_eq!(request.side_effect, SideEffect::None);
        assert_eq!(request.resource, "q");
    }
}
