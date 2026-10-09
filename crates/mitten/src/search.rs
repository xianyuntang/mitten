//! `web_search` tool: queries a user-hosted SearXNG instance through its JSON API.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::header::ACCEPT;
use rig_core::completion::ToolDefinition;
use serde_json::{Value, json};

use crate::config::Searxng;

const TIMEOUT: Duration = Duration::from_secs(15);
/// Snippet characters kept per result; titles and URLs are kept whole.
const MAX_SNIPPET_CHARS: usize = 300;
/// How long a search's results are reused for the same search, sparing SearXNG's upstream engines.
const CACHE_TTL: Duration = Duration::from_secs(600);

/// System prompt section, included only when `web_search` is available.
pub const GUIDANCE: &str = "\
# Web search
Before answering any question that asks for facts, news, prices, versions, docs, \
recommendations, or anything that may have changed, call web_search first, even if you think you \
already know the answer; your memory may be outdated. Skip it only for small talk, thanks, or \
tasks that are purely about local files, memory, or settings. Most questions need 2 to 4 \
searches: stop once the results answer the question, and search more only when they are thin or \
disagree. Start broad, then rephrase: different keywords and synonyms, more specific \
terms (names, versions, error messages, dates), both English and the user's language, and \
site:domain or \"exact phrase\" to target good sources. Compare what the results say and prefer \
official or primary sources. For news or anything recent, set time_range (day, week, month, \
year) and category news, and check each result's date against today's. For a specific past date \
or span (\"last Wednesday\", \"in June\"), work out the dates from today's and set after and before \
instead. For figures on a given date (prices, rates, weather, results), prefer the primary source \
that publishes them (an exchange, central bank, or agency) and read it with fetch_url over news \
snippets. If results are still thin or disagree, search again with new queries \
instead of guessing. Snippets are short excerpts; when you need a page's details or the snippets \
look thin, read the most promising results with fetch_url. Name the URLs your answer relies on.";

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "web_search".to_owned(),
        description: "Search the web for current information. Returns titles, URLs, and snippets. \
                      Operators like site:example.com and \"exact phrase\" usually work. \
                      Rephrase rather than repeat a query; the same search again returns the earlier results."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to search for."},
                "time_range": {
                    "type": "string",
                    "enum": ["day", "week", "month", "year"],
                    "description": "Only results from this recent period; omit for any time.",
                },
                "category": {
                    "type": "string",
                    "enum": ["general", "news"],
                    "description": "news searches news sites; default general.",
                },
                "after": {
                    "type": "string",
                    "description": "Only results published on or after this date, YYYY-MM-DD. Approximate; overrides time_range and category.",
                },
                "before": {
                    "type": "string",
                    "description": "Only results published before this date, YYYY-MM-DD. Approximate; overrides time_range and category.",
                },
            },
            "required": ["query"],
        }),
    }
}

#[derive(Debug)]
pub struct WebSearch {
    client: reqwest::Client,
    config: Searxng,
    /// Formatted results by search, with when they were fetched.
    // ponytail: in memory, per conversation; share across conversations if repeats span them.
    cache: Mutex<HashMap<String, (Instant, String)>>,
}

impl WebSearch {
    pub fn new(config: Searxng) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("mitten/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            client,
            config,
            cache: Mutex::default(),
        })
    }

    /// Runs one tool call; failures come back as text for the model.
    pub async fn run(&self, args: &Value) -> String {
        let Some(query) = args["query"]
            .as_str()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        else {
            return "error: `query` is required".to_owned();
        };
        // Only known values reach SearXNG; anything else falls back to its default.
        let pick = |key: &str, allowed: &[&'static str]| {
            args[key]
                .as_str()
                .and_then(|v| allowed.iter().find(|a| **a == v).copied())
        };
        let mut time_range = pick("time_range", &["day", "week", "month", "year"]);
        let mut category = pick("category", &["general", "news"]);
        let dates = match date_operators(args) {
            Ok(dates) => dates,
            Err(problem) => return format!("error: {problem}"),
        };
        let query = if dates.is_empty() {
            query.to_owned()
        } else {
            // SearXNG has no date range; Google reads these from the query. News engines (Bing)
            // return nothing with them, so the search goes to general engines only.
            time_range = None;
            category = Some("general");
            format!("{query} {dates}")
        };
        let key = format!("{query}\n{time_range:?}\n{category:?}");
        if let Some(results) = self.cached(&key) {
            return format!("(same search as before, results reused)\n{results}");
        }
        match self.fetch(&query, time_range, category).await {
            Ok(body) => {
                let results = format_results(&body, self.config.results);
                if let Ok(mut cache) = self.cache.lock() {
                    cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
                    cache.insert(key, (Instant::now(), results.clone()));
                }
                results
            }
            Err(err) => format!("error: {err:#}"),
        }
    }

    /// Results of the same search made within `CACHE_TTL`.
    fn cached(&self, key: &str) -> Option<String> {
        let cache = self.cache.lock().ok()?;
        let (at, results) = cache.get(key)?;
        (at.elapsed() < CACHE_TTL).then(|| results.clone())
    }

    async fn fetch(
        &self,
        query: &str,
        time_range: Option<&str>,
        category: Option<&str>,
    ) -> Result<Value> {
        let params = [("q", Some(query)), ("format", Some("json"))]
            .into_iter()
            .chain([("time_range", time_range), ("categories", category)])
            .filter_map(|(key, value)| Some((key, value?)));
        let url = reqwest::Url::parse_with_params(&format!("{}/search", self.config.url), params)
            .context("invalid SearXNG URL")?;
        let mut request = self.client.get(url).header(ACCEPT, "application/json");
        if let Some((user, password)) = &self.config.auth {
            request = request.basic_auth(user, Some(password.as_str()));
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("could not reach SearXNG at {}", self.config.url))?;
        match response.status() {
            status if status.is_success() => {}
            StatusCode::UNAUTHORIZED => {
                bail!("SearXNG rejected the username or password (HTTP 401)")
            }
            StatusCode::FORBIDDEN => bail!(
                "SearXNG refused the request (HTTP 403): `json` may be missing from \
                 search.formats in its settings.yml, or its limiter blocked this client"
            ),
            StatusCode::TOO_MANY_REQUESTS => bail!("SearXNG rate-limited the request (HTTP 429)"),
            status => bail!("SearXNG returned HTTP {status}"),
        }
        response
            .json()
            .await
            .context("SearXNG did not return JSON; is `json` in search.formats?")
    }
}

/// `after:` / `before:` query operators from the call's dates, empty if neither is given.
fn date_operators(args: &Value) -> std::result::Result<String, String> {
    let mut operators = Vec::new();
    for key in ["after", "before"] {
        let Some(text) = args[key].as_str().map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .map_err(|_| format!("`{key}` must be a date like 2026-09-30, not {text:?}"))?;
        operators.push(format!("{key}:{date}"));
    }
    Ok(operators.join(" "))
}

/// The top `limit` results by SearXNG's score, as numbered title / URL (and date) / snippet blocks.
fn format_results(body: &Value, limit: usize) -> String {
    let score = |r: &&Value| r["score"].as_f64().unwrap_or(0.0);
    let mut results: Vec<&Value> = body["results"]
        .as_array()
        .map(|rows| rows.iter().collect())
        .unwrap_or_default();
    results.sort_by(|a, b| score(b).total_cmp(&score(a)));
    let blocks: Vec<String> = results
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, r)| {
            let title = r["title"].as_str().unwrap_or("(untitled)").trim();
            let mut url = r["url"].as_str().unwrap_or_default().to_owned();
            // `publishedDate` is ISO 8601; the day is enough to judge freshness.
            if let Some(date) = r["publishedDate"].as_str().and_then(|d| d.get(..10)) {
                url.push_str(&format!(" · {date}"));
            }
            let snippet: String = r["content"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .chars()
                .take(MAX_SNIPPET_CHARS)
                .collect();
            format!("{}. {title}\n   {url}\n   {snippet}", i + 1)
        })
        .collect();
    if blocks.is_empty() {
        "no results".to_owned()
    } else {
        blocks.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_results_ranks_by_score_and_caps() {
        let body = json!({"results": [
            {"title": "low", "url": "https://a", "content": "x", "score": 0.5},
            {"title": " high ", "url": "https://b", "content": "y".repeat(400), "score": 2.0},
            {"title": "mid", "url": "https://c", "score": 1.0, "publishedDate": "2026-10-04T08:00:00"},
        ]});
        let text = format_results(&body, 2);
        assert!(text.starts_with("1. high\n   https://b\n   yyy"), "{text}");
        assert!(
            text.contains("2. mid\n   https://c · 2026-10-04\n"),
            "{text}"
        );
        assert!(!text.contains("low"));
        assert!(!text.contains(&"y".repeat(MAX_SNIPPET_CHARS + 1)));
        assert_eq!(format_results(&json!({"results": []}), 5), "no results");
        assert_eq!(format_results(&json!({}), 5), "no results");
    }

    /// Serves one canned HTTP response and hands back the raw request it received.
    async fn one_shot_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0; 4096];
            let n = socket.read(&mut buf).await.expect("read");
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.expect("write");
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (url, handle)
    }

    fn searxng(url: String, auth: bool) -> WebSearch {
        let auth = auth.then(|| {
            let password = serde_json::from_value(json!("pw")).expect("secret");
            ("me".to_owned(), password)
        });
        WebSearch::new(Searxng {
            url,
            auth,
            results: 5,
        })
        .expect("client")
    }

    #[tokio::test]
    async fn sends_basic_auth_and_encoded_query() {
        let (url, server) = one_shot_server(
            "200 OK",
            r#"{"results":[{"title":"t","url":"https://u","content":"c"}]}"#,
        )
        .await;
        let out = searxng(url, true).run(&json!({"query": "rust 磁碟"})).await;
        assert_eq!(out, "1. t\n   https://u\n   c");
        let request = server.await.expect("server");
        assert!(
            request.starts_with("GET /search?q=rust+%E7%A3%81%E7%A2%9F&format=json "),
            "{request}"
        );
        // base64 of "me:pw"
        assert!(
            request
                .to_lowercase()
                .contains("authorization: basic bwu6chc="),
            "{request}"
        );
    }

    #[tokio::test]
    async fn passes_known_time_range_and_category_only() {
        let (url, server) = one_shot_server("200 OK", r#"{"results":[]}"#).await;
        let args = json!({"query": "x", "time_range": "week", "category": "news"});
        assert_eq!(searxng(url, false).run(&args).await, "no results");
        let request = server.await.expect("server");
        assert!(
            request.starts_with("GET /search?q=x&format=json&time_range=week&categories=news "),
            "{request}"
        );
        let (url, server) = one_shot_server("200 OK", r#"{"results":[]}"#).await;
        let args = json!({"query": "x", "time_range": "decade"});
        searxng(url, false).run(&args).await;
        let request = server.await.expect("server");
        assert!(
            request.starts_with("GET /search?q=x&format=json "),
            "{request}"
        );
    }

    #[tokio::test]
    async fn dates_become_query_operators_on_general_engines() {
        let (url, server) = one_shot_server("200 OK", r#"{"results":[]}"#).await;
        let args = json!({
            "query": "taiex",
            "after": "2026-09-29",
            "before": "2026-10-01",
            "category": "news",
            "time_range": "week",
        });
        searxng(url, false).run(&args).await;
        let request = server.await.expect("server");
        assert!(
            request.starts_with(
                "GET /search?q=taiex+after%3A2026-09-29+before%3A2026-10-01&format=json&categories=general "
            ),
            "{request}"
        );
        let bad = json!({"query": "x", "after": "yesterday"});
        let out = searxng("http://127.0.0.1:1".to_owned(), false)
            .run(&bad)
            .await;
        assert!(out.contains("`after` must be a date"), "{out}");
    }

    #[tokio::test]
    async fn repeated_search_is_served_from_cache() {
        let (url, _server) = one_shot_server(
            "200 OK",
            r#"{"results":[{"title":"t","url":"https://u","content":"c"}]}"#,
        )
        .await;
        let search = searxng(url, false);
        assert_eq!(
            search.run(&json!({"query": "x"})).await,
            "1. t\n   https://u\n   c"
        );
        // The one-shot server is gone, so this answer can only come from the cache.
        let again = search.run(&json!({"query": " x "})).await;
        assert!(again.starts_with("(same search as before"), "{again}");
        assert!(again.ends_with("1. t\n   https://u\n   c"), "{again}");
        let other = search
            .run(&json!({"query": "x", "time_range": "day"}))
            .await;
        assert!(
            other.starts_with("error:"),
            "different parameters miss: {other}"
        );
    }

    #[tokio::test]
    async fn explains_auth_and_json_failures() {
        let (url, _server) = one_shot_server("401 Unauthorized", "").await;
        let out = searxng(url, true).run(&json!({"query": "x"})).await;
        assert!(out.contains("username or password"), "{out}");
        let (url, _server) = one_shot_server("403 Forbidden", "").await;
        let out = searxng(url, false).run(&json!({"query": "x"})).await;
        assert!(out.contains("search.formats"), "{out}");
    }
}
