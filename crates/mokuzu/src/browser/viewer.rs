//! Live view of mokuzu's Chrome for the user: a small web page on the local network that shows
//! a tab and passes clicks, scrolling, and typing back to it, for the steps a page wants a person
//! for (a captcha, a sign-in, a code). The link carries a random token; without it every request
//! is refused.

use std::collections::HashMap;
use std::net::{IpAddr, UdpSocket};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventType, DispatchMouseEventType, InsertTextParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
    StopScreencastParams,
};
use chromiumoxide::page::Page;
use futures::StreamExt;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::OnceCell;

use super::{
    LOAD_LIMIT, REQUEST_TIMEOUT, SCREEN, SESSION, evaluate, key_event, mouse, random, send_key,
};

/// Largest request body taken, enough for a pasted paragraph.
const MAX_BODY: usize = 64 * 1024;

/// Tabs the viewer can show, copied from the session after every browser call, so the viewer
/// keeps working while a call holds the session waiting on a confirm.
pub(super) static TABS: std::sync::Mutex<Vec<(u32, Page)>> = std::sync::Mutex::new(Vec::new());

/// The viewer's link, starting it on first use; it then serves until mokuzu exits.
// ponytail: never stops once started; stop it with the session if an open port bothers anyone.
pub(super) async fn link() -> Result<String> {
    static LINK: OnceCell<String> = OnceCell::const_new();
    LINK.get_or_try_init(start).await.cloned()
}

async fn start() -> Result<String> {
    let listener = TcpListener::bind(("0.0.0.0", 0))
        .await
        .context("failed to open the viewer's port")?;
    let port = listener.local_addr()?.port();
    // ponytail: 128 bits from std's OS-seeded hasher keys; switch to getrandom if this ever
    // guards more than a LAN page.
    let token = format!("{:016x}{:016x}", random(), random());
    let host = lan_ip().map_or_else(|| "localhost".to_owned(), |ip| ip.to_string());
    let expected = token.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let token = expected.clone();
            tokio::spawn(async move {
                if let Err(err) = serve(stream, &token).await {
                    tracing::debug!("viewer request failed: {err:#}");
                }
            });
        }
    });
    tracing::debug!(port, "browser viewer is up");
    Ok(format!("http://{host}:{port}/?token={token}"))
}

/// The address other machines on the network reach this one at: the one the default route
/// leaves from. Connecting a UDP socket sends nothing.
fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// Answers one HTTP/1.1 request, then closes the connection.
async fn serve(connection: TcpStream, token: &str) -> Result<()> {
    let (read, mut write) = connection.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        bail!("bad request line {line:?}");
    };
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await? == 0 || header.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse()?;
        }
    }
    if length > MAX_BODY {
        bail!("request body of {length} bytes");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;

    let url = Url::parse(&format!("http://viewer{target}"))?;
    let query: HashMap<_, _> = url.query_pairs().collect();
    let allowed = query
        .get("token")
        .is_some_and(|given| same(given.as_bytes(), token.as_bytes()));
    let tab = query.get("tab").and_then(|tab| tab.parse().ok());
    if allowed && method == "GET" && url.path() == "/stream" {
        return stream(&page(tab)?, &mut write).await;
    }
    let (status, kind, content) = if allowed {
        match respond(method, url.path(), tab, &body).await {
            Ok(Some(reply)) => reply,
            Ok(None) => ("404 Not Found", "text/plain", b"not found".to_vec()),
            Err(err) => (
                "500 Internal Server Error",
                "text/plain",
                format!("{err:#}").into_bytes(),
            ),
        }
    } else {
        (
            "403 Forbidden",
            "text/plain",
            b"wrong or missing token".to_vec(),
        )
    };
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n\r\n",
        content.len()
    );
    write.write_all(head.as_bytes()).await?;
    write.write_all(&content).await?;
    Ok(())
}

type Reply = (&'static str, &'static str, Vec<u8>);

async fn respond(method: &str, path: &str, tab: Option<u32>, body: &[u8]) -> Result<Option<Reply>> {
    let ok = "200 OK";
    Ok(Some(match (method, path) {
        ("GET", "/") => (ok, "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
        ("GET", "/tabs") => (
            ok,
            "application/json",
            tabs().await.to_string().into_bytes(),
        ),
        ("POST", "/input") => {
            let input: Input = serde_json::from_slice(body).context("bad input")?;
            apply(&page(tab)?, input).await?;
            (ok, "text/plain", Vec::new())
        }
        _ => return Ok(None),
    }))
}

/// Streams `page` as MJPEG until the viewer goes. Chrome pushes a frame whenever the page
/// repaints, so the picture follows input without polling.
// ponytail: one screencast per tab; a second viewer on the same tab stops the first one's
// when it leaves. Track viewers per tab if two people ever watch at once.
async fn stream(page: &Page, write: &mut OwnedWriteHalf) -> Result<()> {
    write
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: multipart/x-mixed-replace; boundary=frame\r\n\
              cache-control: no-store\r\nconnection: close\r\n\r\n",
        )
        .await?;
    // The page as it is now: Chrome sends nothing until it next repaints.
    frame(write, &super::shot(page).await?).await?;
    let mut frames = page.event_listener::<EventScreencastFrame>().await?;
    let start = StartScreencastParams::builder()
        .format(StartScreencastFormat::Jpeg)
        // Smaller frames: on a Pi without a GPU, encoding them is most of each frame's cost.
        .quality(50)
        .max_width(SCREEN.0 * 3 / 4)
        .max_height(SCREEN.1 * 3 / 4)
        .build();
    page.execute(start).await?;
    let streamed = async {
        while let Some(event) = frames.next().await {
            // Acked on arrival, so Chrome renders the next frame while this one is sent. A slow
            // viewer still holds Chrome back: the next frame is not read, so not acked, until
            // this one is written.
            page.execute(ScreencastFrameAckParams::new(event.session_id))
                .await?;
            let jpeg = base64::engine::general_purpose::STANDARD
                .decode(AsRef::<str>::as_ref(&event.data))?;
            frame(write, &jpeg).await?;
        }
        Ok(())
    }
    .await;
    let _ = page.execute(StopScreencastParams::default()).await;
    streamed
}

async fn frame(write: &mut OwnedWriteHalf, jpeg: &[u8]) -> Result<()> {
    let head = format!(
        "--frame\r\ncontent-type: image/jpeg\r\ncontent-length: {}\r\n\r\n",
        jpeg.len()
    );
    write.write_all(head.as_bytes()).await?;
    write.write_all(jpeg).await?;
    write.write_all(b"\r\n").await?;
    Ok(())
}

/// Constant-time comparison, so response timing gives the token away a byte at a time.
fn same(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// Open tabs with titles; first syncs with Chrome when no browser call is running, so popups
/// the user opened here show up.
async fn tabs() -> Value {
    if let Ok(mut session) = SESSION.try_lock()
        && let Some(session) = session.as_mut()
        && tokio::time::timeout(REQUEST_TIMEOUT, session.refresh())
            .await
            .is_ok_and(|refreshed| refreshed.is_ok())
    {
        publish(&session.tabs);
    }
    let tabs = TABS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut list = Vec::new();
    for (id, page) in tabs {
        let title = page.get_title().await.ok().flatten().unwrap_or_default();
        list.push(json!({"id": id, "title": title}));
    }
    Value::Array(list)
}

pub(super) fn publish(tabs: &[(u32, Page)]) {
    *TABS.lock().unwrap_or_else(|e| e.into_inner()) = tabs.to_vec();
}

fn page(tab: Option<u32>) -> Result<Page> {
    let tab = tab.context("`tab` is required")?;
    TABS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(id, _)| *id == tab)
        .map(|(_, page)| page.clone())
        .with_context(|| format!("no tab {tab}"))
}

/// What the user did on the page. Positions are fractions of the shown screenshot.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Input {
    Click { x: f64, y: f64 },
    Wheel { x: f64, y: f64, dy: f64 },
    Key { key: String },
    Text { text: String },
    Goto { url: String },
    Back,
}

async fn apply(page: &Page, input: Input) -> Result<()> {
    match input {
        Input::Click { x, y } => {
            let at = point(page, x, y).await?;
            mouse(page, DispatchMouseEventType::MouseMoved, at, None).await?;
            mouse(page, DispatchMouseEventType::MousePressed, at, Some(1)).await?;
            mouse(page, DispatchMouseEventType::MouseReleased, at, Some(0)).await?;
        }
        Input::Wheel { x, y, dy } => super::wheel(page, point(page, x, y).await?, dy).await?,
        Input::Key { key } => {
            let Some((code, virtual_key)) = special_key(&key) else {
                return Ok(());
            };
            let mut down = key_event(DispatchKeyEventType::RawKeyDown, &key, code, virtual_key, 0);
            if key == "Enter" {
                down =
                    key_event(DispatchKeyEventType::KeyDown, &key, code, virtual_key, 0).text("\r");
            }
            send_key(page, down).await?;
            send_key(
                page,
                key_event(DispatchKeyEventType::KeyUp, &key, code, virtual_key, 0),
            )
            .await?;
        }
        Input::Text { text } => {
            page.execute(InsertTextParams::new(text)).await?;
        }
        Input::Goto { url } => {
            let url = Url::parse(url.trim()).context("not a valid URL")?;
            if !matches!(url.scheme(), "http" | "https") {
                bail!("only http and https URLs can be opened");
            }
            tokio::time::timeout(LOAD_LIMIT, page.goto(url.as_str()))
                .await
                .map_err(|_| anyhow!("the page did not finish loading"))??;
        }
        Input::Back => {
            let _: bool = evaluate(page, "history.back(); return true;").await?;
        }
    }
    Ok(())
}

/// The viewport position at fractions `x`, `y` of it.
async fn point(page: &Page, x: f64, y: f64) -> Result<(f64, f64)> {
    let size: Vec<f64> = evaluate(page, "return [innerWidth, innerHeight];").await?;
    let [width, height] = size[..] else {
        bail!("the page reported no size");
    };
    Ok((x.clamp(0.0, 1.0) * width, y.clamp(0.0, 1.0) * height))
}

/// The `code` and Windows virtual key of keys that do something besides typing a character.
fn special_key(key: &str) -> Option<(&'static str, i64)> {
    Some(match key {
        "Enter" => ("Enter", 13),
        "Backspace" => ("Backspace", 8),
        "Tab" => ("Tab", 9),
        "Escape" => ("Escape", 27),
        "Delete" => ("Delete", 46),
        "ArrowLeft" => ("ArrowLeft", 37),
        "ArrowUp" => ("ArrowUp", 38),
        "ArrowRight" => ("ArrowRight", 39),
        "ArrowDown" => ("ArrowDown", 40),
        _ => return None,
    })
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>mokuzu browser</title>
<style>
  :root { color-scheme: light dark; --bg: #f4f4f5; --fg: #18181b; --line: #d4d4d8; }
  @media (prefers-color-scheme: dark) { :root { --bg: #18181b; --fg: #f4f4f5; --line: #3f3f46; } }
  body { margin: 0; background: var(--bg); color: var(--fg); font: 14px system-ui, sans-serif; }
  .bar { display: flex; gap: 6px; padding: 8px; }
  .bar input, .bar select, .bar button { font: inherit; padding: 6px 8px; border: 1px solid var(--line);
    border-radius: 6px; background: transparent; color: inherit; min-width: 0; }
  .grow { flex: 1; }
  #screen { display: block; width: 100%; outline: none; cursor: default; }
  #screen:focus { box-shadow: 0 0 0 2px #3b82f6 inset; }
  #status { padding: 0 8px 8px; opacity: .7; }
</style>
<div class="bar">
  <select id="tab" aria-label="Tab"></select>
  <button id="back" title="Back">←</button>
  <input id="url" class="grow" placeholder="Open a URL (Enter)" inputmode="url">
</div>
<img id="screen" tabindex="0" alt="Chrome tab">
<div class="bar">
  <input id="text" class="grow" placeholder="Type text here, then Enter to send it to the page">
</div>
<div id="status">Click the picture to use the page; keys go to it while it has focus.</div>
<script>
const token = new URLSearchParams(location.search).get('token');
const api = (path, tab) => `${path}?token=${token}` + (tab == null ? '' : `&tab=${tab}`);
const $ = id => document.getElementById(id);
let tab = null;
const status = text => $('status').textContent = text;
const sleep = ms => new Promise(done => setTimeout(done, ms));

async function tabs() {
  const list = await (await fetch(api('/tabs'))).json();
  const shown = tab;
  if (!list.some(t => t.id === tab)) tab = list.at(-1)?.id ?? null;
  if (tab !== shown) show();
  $('tab').replaceChildren(...list.map(t => new Option(`${t.id}: ${t.title || 'untitled'}`, t.id, false, t.id === tab)));
}
$('tab').onchange = () => { tab = Number($('tab').value); show(); };

async function send(input) {
  if (tab == null) return;
  const reply = await fetch(api('/input', tab), { method: 'POST', body: JSON.stringify(input) });
  if (!reply.ok) status(await reply.text());
}

// Chrome pushes a frame on every repaint over one long response the browser shows as it comes.
function show() {
  if (tab == null) { status('No tab open yet.'); $('screen').removeAttribute('src'); return; }
  $('screen').src = api('/stream', tab);
}
$('screen').onerror = () => setTimeout(() => tabs().then(show).catch(() => {}), 1000);

const at = e => { const r = $('screen').getBoundingClientRect(); return { x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height }; };
$('screen').addEventListener('click', e => { $('screen').focus(); send({ kind: 'click', ...at(e) }); });
$('screen').addEventListener('wheel', e => { e.preventDefault(); send({ kind: 'wheel', ...at(e), dy: e.deltaY }); }, { passive: false });
$('screen').addEventListener('keydown', e => {
  if (e.metaKey || e.ctrlKey || e.altKey || e.isComposing) return;
  e.preventDefault();
  send(e.key.length === 1 ? { kind: 'text', text: e.key } : { kind: 'key', key: e.key });
});
$('text').addEventListener('keydown', e => {
  if (e.key !== 'Enter' || e.isComposing) return;
  send({ kind: 'text', text: $('text').value });
  $('text').value = '';
});
$('url').addEventListener('keydown', e => {
  if (e.key !== 'Enter' || e.isComposing) return;
  status('Opening…');
  send({ kind: 'goto', url: $('url').value }).then(() => status('Opened.'));
});
$('back').onclick = () => send({ kind: 'back' });
setInterval(() => tabs().catch(() => {}), 3000);
tabs().catch(() => status('Cannot reach mokuzu.'));
</script>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abd", b"abc"));
        assert!(!same(b"ab", b"abc"));
    }

    #[tokio::test]
    async fn requests_need_the_token() {
        // Its own viewer: `link`'s dies with the first test runtime that started it.
        let link = start().await.expect("viewer starts");
        let client = reqwest::Client::new();
        let page = client.get(&link).send().await.expect("page");
        assert_eq!(page.status(), 200);
        assert!(page.text().await.expect("body").contains("mokuzu browser"));
        let tabs = link.replace("/?", "/tabs?");
        let tabs = client.get(&tabs).send().await.expect("tabs");
        assert_eq!(tabs.text().await.expect("body"), "[]");
        let base = link.split('?').next().expect("base");
        let refused = client.get(base).send().await.expect("refused");
        assert_eq!(refused.status(), 403);
    }
}
