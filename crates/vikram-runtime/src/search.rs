//! Web search backends behind `web.search`.
//!
//! Two real backends share one interface: Bing's HTML endpoint, which needs no key, and Brave's
//! JSON API, which is used only when a key is configured. The backend is chosen by
//! [`SearchConfig`]; a failure is returned as a [`SearchError`] naming the HTTP status, the
//! missing key, or the unreachable host. There is deliberately no empty-success path: a response
//! the parser cannot read is a failure, not "no results".

use crate::config::{SearchBackend, SearchConfig};
use serde_json::Value;
use std::time::Duration;

/// The most results a single call may return. The model asks for fewer; this caps a bad ask.
pub const MAX_RESULTS: usize = 10;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(
        "{backend} search needs a key: set `{env}` in the environment or $VIKRAM_HOME/.env, or set `search.backend` to \"bing\""
    )]
    MissingKey { backend: &'static str, env: String },
    #[error("{backend} search request failed: {reason}")]
    Request {
        backend: &'static str,
        reason: String,
    },
    #[error("{backend} search returned HTTP {status}: {detail}")]
    Status {
        backend: &'static str,
        status: u16,
        detail: String,
    },
    #[error("{backend} search returned a response that could not be read: {reason}")]
    Body {
        backend: &'static str,
        reason: String,
    },
    #[error("{backend} search returned no usable results: {detail}")]
    NoResults {
        backend: &'static str,
        detail: String,
    },
}

/// Run one search against the configured backend.
pub fn search(
    config: &SearchConfig,
    api_key: Option<&str>,
    client: &reqwest::blocking::Client,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, SearchError> {
    let limit = limit.clamp(1, MAX_RESULTS);
    match config.backend {
        SearchBackend::Brave => {
            let key = api_key.ok_or_else(|| SearchError::MissingKey {
                backend: "brave",
                env: config.api_key_env.clone(),
            })?;
            brave(client, &config.brave_endpoint, key, query, limit)
        }
        SearchBackend::Bing => bing(client, &config.bing_endpoint, query, limit),
        SearchBackend::Auto => match api_key {
            Some(key) => brave(client, &config.brave_endpoint, key, query, limit),
            None => bing(client, &config.bing_endpoint, query, limit),
        },
    }
}

/// Render results in the shape the model already reads for other tools.
pub fn render(results: &[SearchResult]) -> String {
    let mut content = String::new();
    for (i, result) in results.iter().enumerate() {
        content.push_str(&format!(
            "{}. {}\n   {}\n   URL: {}\n",
            i + 1,
            result.title,
            result.snippet,
            result.url
        ));
    }
    content
}

fn bing(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, SearchError> {
    let url = format!("{endpoint}?q={}&count={limit}&setlang=en", encode(query));
    let body = get(client, "bing", &url, "text/html,application/xhtml+xml")?;
    parse_bing(&body, limit)
}

fn brave(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    key: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, SearchError> {
    let url = format!("{endpoint}?q={}&count={limit}", encode(query));
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .header("X-Subscription-Token", key)
        .send()
        .map_err(|err| SearchError::Request {
            backend: "brave",
            reason: err.to_string(),
        })?;
    let status = response.status();
    let body = response.text().map_err(|err| SearchError::Body {
        backend: "brave",
        reason: err.to_string(),
    })?;
    if !status.is_success() {
        return Err(SearchError::Status {
            backend: "brave",
            status: status.as_u16(),
            detail: detail(&body),
        });
    }
    parse_brave(&body, limit)
}

fn get(
    client: &reqwest::blocking::Client,
    backend: &'static str,
    url: &str,
    accept: &str,
) -> Result<String, SearchError> {
    let response = client
        .get(url)
        .header("Accept", accept)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .map_err(|err| SearchError::Request {
            backend,
            reason: err.to_string(),
        })?;
    let status = response.status();
    let body = response.text().map_err(|err| SearchError::Body {
        backend,
        reason: err.to_string(),
    })?;
    if !status.is_success() {
        return Err(SearchError::Status {
            backend,
            status: status.as_u16(),
            detail: detail(&body),
        });
    }
    Ok(body)
}

/// A short, single-line excerpt of an error body, so a status code is never bare.
fn detail(body: &str) -> String {
    let collapsed = collapse(body);
    if collapsed.is_empty() {
        return "empty body".into();
    }
    collapsed.chars().take(200).collect()
}

pub(crate) fn parse_bing(body: &str, limit: usize) -> Result<Vec<SearchResult>, SearchError> {
    let document = scraper::Html::parse_document(body);
    let container = selector("li.b_algo");
    let title_link = selector("h2 a");
    let snippet_node = selector("div.b_caption p");
    let any_paragraph = selector("p");
    let cite = selector("cite");

    let mut results: Vec<SearchResult> = Vec::new();
    for item in document.select(&container) {
        let Some(anchor) = item.select(&title_link).next() else {
            continue;
        };
        let title = collapse(&anchor.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let mut url = decode_bing_href(anchor.value().attr("href").unwrap_or_default());
        if url.is_empty() {
            url = item
                .select(&cite)
                .next()
                .map(|e| collapse(&e.text().collect::<String>()))
                .unwrap_or_default();
        }
        if !is_http_url(&url) {
            continue;
        }
        let snippet = item
            .select(&snippet_node)
            .next()
            .or_else(|| item.select(&any_paragraph).next())
            .map(|e| collapse(&e.text().collect::<String>()))
            .unwrap_or_default();
        push_unique(&mut results, title, url, snippet, limit);
        if results.len() >= limit {
            break;
        }
    }

    if results.is_empty() {
        return Err(SearchError::NoResults {
            backend: "bing",
            detail: format!(
                "the endpoint answered with {} bytes but no `li.b_algo` block could be read; a bot challenge or changed markup is likely",
                body.len()
            ),
        });
    }
    Ok(results)
}

pub(crate) fn parse_brave(body: &str, limit: usize) -> Result<Vec<SearchResult>, SearchError> {
    let json: Value = serde_json::from_str(body).map_err(|err| SearchError::Body {
        backend: "brave",
        reason: err.to_string(),
    })?;
    let entries = json
        .pointer("/web/results")
        .and_then(Value::as_array)
        .ok_or_else(|| SearchError::Body {
            backend: "brave",
            reason: "the response has no `web.results` array".into(),
        })?;

    let mut results: Vec<SearchResult> = Vec::new();
    for entry in entries {
        let title = entry
            .get("title")
            .and_then(Value::as_str)
            .map(|t| collapse(&text_of_fragment(t)))
            .unwrap_or_default();
        let url = entry
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if title.is_empty() || !is_http_url(&url) {
            continue;
        }
        let snippet = entry
            .get("description")
            .and_then(Value::as_str)
            .map(text_of_fragment)
            .unwrap_or_default();
        push_unique(&mut results, title, url, snippet, limit);
        if results.len() >= limit {
            break;
        }
    }

    if results.is_empty() {
        return Err(SearchError::NoResults {
            backend: "brave",
            detail: "the API answered with an empty `web.results` array".into(),
        });
    }
    Ok(results)
}

/// Bing wraps outbound links in `https://www.bing.com/ck/a?...&u=a1<base64url>`; the payload is the
/// real target. Direct links are returned unchanged.
fn decode_bing_href(href: &str) -> String {
    let href = href.trim();
    if href.is_empty() {
        return String::new();
    }
    let plain = href.replace("&amp;", "&");
    let is_bing_redirect = plain.contains("bing.com/ck/a") || plain.contains("/ck/a?");
    if let Some(pos) = plain.find("u=a1") {
        let payload = plain[pos + 2..]
            .split('&')
            .next()
            .unwrap_or_default()
            .strip_prefix("a1")
            .unwrap_or_default();
        if let Some(decoded) = base64url_decode(payload)
            && is_http_url(&decoded)
        {
            return decoded;
        }
    }
    if is_bing_redirect {
        return String::new();
    }
    if plain.starts_with("//") {
        return format!("https:{plain}");
    }
    if plain.starts_with('/') {
        return format!("https://www.bing.com{plain}");
    }
    plain
}

fn base64url_decode(input: &str) -> Option<String> {
    let mut bits: u32 = 0;
    let mut pending: u32 = 0;
    let mut bytes = Vec::with_capacity(input.len() * 3 / 4);
    for byte in input.bytes() {
        let value = (match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        }) as u32;
        bits = (bits << 6) | value;
        pending += 6;
        if pending >= 8 {
            pending -= 8;
            bytes.push((bits >> pending) as u8);
            bits &= (1 << pending) - 1;
        }
    }
    String::from_utf8(bytes).ok()
}

fn push_unique(
    results: &mut Vec<SearchResult>,
    title: String,
    url: String,
    snippet: String,
    limit: usize,
) {
    if results.len() >= limit || results.iter().any(|r| r.url == url) {
        return;
    }
    results.push(SearchResult {
        title,
        url,
        snippet,
    });
}

fn selector(raw: &str) -> scraper::Selector {
    scraper::Selector::parse(raw).expect("static selector")
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Brave returns HTML fragments in `description`; the model must see text, not tags.
fn text_of_fragment(fragment: &str) -> String {
    if !fragment.contains('<') {
        return collapse(fragment);
    }
    let document = scraper::Html::parse_fragment(fragment);
    collapse(&document.root_element().text().collect::<String>())
}

fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Shared client for both backends: one user agent, bounded connect and total time.
pub fn client() -> Result<reqwest::blocking::Client, SearchError> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|err| SearchError::Request {
            backend: "web",
            reason: err.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BING_PAGE: &str = r#"
        <html><body><ol id="b_results">
          <li class="b_algo">
            <h2><a href="https://www.bing.com/ck/a?!&amp;&amp;p=abc&amp;u=a1aHR0cHM6Ly93d3cucnVzdC1sYW5nLm9yZy8&amp;ntb=1">
              Rust <strong>Programming</strong> Language
            </a></h2>
            <div class="b_caption"><p class="b_lineclamp2">Rust is blazingly fast and memory-efficient.</p></div>
          </li>
          <li class="b_algo">
            <h2><a href="https://doc.rust-lang.org/book/">The Rust Book</a></h2>
            <div class="b_caption"><p>A hands-on guide.</p></div>
          </li>
        </ol></body></html>
    "#;

    const BRAVE_JSON: &str = r#"{
        "web": {"results": [
            {"title": "Rust Programming Language", "url": "https://www.rust-lang.org/",
             "description": "A <strong>fast</strong> and safe language."},
            {"title": "The Rust Book", "url": "https://doc.rust-lang.org/book/",
             "description": "Learn Rust from scratch."}
        ]}
    }"#;

    #[test]
    fn bing_html_parses_into_title_url_and_snippet() {
        let results = parse_bing(BING_PAGE, 5).expect("sample page parses");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(
            results[0].snippet.contains("blazingly fast"),
            "{:?}",
            results[0]
        );
        assert_eq!(results[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn bing_results_are_capped_at_the_requested_limit() {
        let results = parse_bing(BING_PAGE, 1).expect("sample page parses");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn brave_json_parses_and_strips_markup_from_snippets() {
        let results = parse_brave(BRAVE_JSON, 5).expect("sample reply parses");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(results[0].snippet, "A fast and safe language.");
        assert!(!results[0].snippet.contains('<'));
    }

    #[test]
    fn a_challenge_page_is_an_error_not_an_empty_success() {
        let err = parse_bing("<html><body>DuckDuckGo challenge</body></html>", 5)
            .expect_err("a page without result blocks is a failure");
        assert!(
            matches!(
                err,
                SearchError::NoResults {
                    backend: "bing",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn an_empty_brave_reply_is_an_error_not_an_empty_success() {
        let err = parse_brave(r#"{"web":{"results":[]}}"#, 5)
            .expect_err("zero results is reported, not fabricated");
        assert!(
            matches!(
                err,
                SearchError::NoResults {
                    backend: "brave",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_missing_key_names_the_variable_to_set() {
        let config = SearchConfig {
            backend: SearchBackend::Brave,
            api_key_env: "VIKRAM_SEARCH_API_KEY".into(),
            ..SearchConfig::default()
        };
        let client = client().expect("client builds");
        let err = search(&config, None, &client, "rust", 5).expect_err("no key means no call");
        let message = err.to_string();
        assert!(message.contains("VIKRAM_SEARCH_API_KEY"), "{message}");
    }

    #[test]
    fn base64url_redirects_decode_to_the_target_and_bad_payloads_do_not() {
        assert_eq!(
            decode_bing_href(
                "https://www.bing.com/ck/a?!&&u=a1aHR0cHM6Ly9ydXN0LWxhbmcub3JnLw&ntb=1"
            ),
            "https://rust-lang.org/"
        );
        assert_eq!(
            decode_bing_href("https://www.bing.com/ck/a?!&&u=%%%&ntb=1"),
            ""
        );
    }
}
