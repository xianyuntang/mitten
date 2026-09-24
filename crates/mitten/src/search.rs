//! `web_search` tool: queries a user-hosted SearXNG instance through its JSON API.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::header::ACCEPT;
use rig_core::completion::ToolDefinition;
use serde_json::{Value, json};

use crate::config::Searxng;

const TIMEOUT: Duration = Duration::from_secs(15);
/// Snippet characters kept per result; titles and URLs are kept whole.
const MAX_SNIPPET_CHARS: usize = 300;

/// System prompt section, included only when `web_search` is available.
pub const GUIDANCE: &str = "\
# Web search
When a question needs information from the web, search several times before answering; one \
search is rarely enough. Start broad, then rephrase: different keywords and synonyms, more specific \
terms (names, versions, error messages, dates), both English and the user's language, and \
site:domain or \"exact phrase\" to target good sources. Compare what the results say and prefer \
official or primary sources. If results are thin or disagree, keep searching with new queries \
instead of guessing. Snippets are short excerpts; when you need a page's details or the snippets \
look thin, read the most promising results with fetch_url. Name the URLs your answer relies on.";

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "web_search".to_owned(),
        description: "Search the web for current information. Returns titles, URLs, and snippets. \
                      Operators like site:example.com and \"exact phrase\" usually work. \
                      Call it several times with rephrased queries rather than relying on one."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {"query": {"type": "string", "description": "What to search for."}},
            "required": ["query"],
        }),
    }
}

#[derive(Debug)]
pub struct WebSearch {
    client: reqwest::Client,
    config: Searxng,
}

impl WebSearch {
    pub fn new(config: Searxng) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("mitten/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { client, config })
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
        match self.fetch(query).await {
            Ok(body) => format_results(&body, self.config.results),
            Err(err) => format!("error: {err:#}"),
        }
    }

    async fn fetch(&self, query: &str) -> Result<Value> {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/search", self.config.url),
            [("q", query), ("format", "json")],
        )
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

/// The top `limit` results by SearXNG's score, as numbered title / URL / snippet blocks.
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
            let url = r["url"].as_str().unwrap_or_default();
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
            {"title": "mid", "url": "https://c", "score": 1.0},
        ]});
        let text = format_results(&body, 2);
        assert!(text.starts_with("1. high\n   https://b\n   yyy"), "{text}");
        assert!(text.contains("2. mid\n   https://c"));
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
    async fn explains_auth_and_json_failures() {
        let (url, _server) = one_shot_server("401 Unauthorized", "").await;
        let out = searxng(url, true).run(&json!({"query": "x"})).await;
        assert!(out.contains("username or password"), "{out}");
        let (url, _server) = one_shot_server("403 Forbidden", "").await;
        let out = searxng(url, false).run(&json!({"query": "x"})).await;
        assert!(out.contains("search.formats"), "{out}");
    }
}
