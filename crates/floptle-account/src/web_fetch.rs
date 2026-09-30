//! One HTTP request from a page, through the browser's own `fetch`.
//!
//! The desktop path is a blocking agent on a worker thread; a page has
//! neither, so this is the same request as an `async` call the caller spawns
//! on the page's event loop. It is what `http.*`, `cloud.*`,
//! `assets.textureFromUrl` and `account.*` all use in a browser build.
//!
//! Three things differ from the desktop, and each is the browser's rule:
//!
//! - **The server has to allow the page's origin (CORS).** A refusal reaches
//!   a page as a bare network error with no detail, so the message names the
//!   likeliest cause.
//! - **Redirects are followed**, and the reply is the last hop's. A page may
//!   not read where a redirect pointed, so there is no `location` to hand on.
//! - **Some headers are the browser's** (`User-Agent`, `Cookie`, `Host`…) and
//!   a page may not set them. Asking is refused by name.
//!
//! No credentials are sent: a page authenticates with the token it holds, not
//! with a cookie the player happens to have for that site.

use wasm_bindgen::JsCast;

/// What came back.
pub struct FetchReply {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

fn js_text(v: &wasm_bindgen::JsValue) -> String {
    v.dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| v.as_string())
        .unwrap_or_else(|| "(no detail)".into())
}

/// Send one request and wait for the whole reply. `timeout_s` aborts it;
/// `max_body` refuses a reply larger than that many bytes.
pub async fn fetch(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
    timeout_s: f64,
    max_body: usize,
) -> Result<FetchReply, String> {
    let win = web_sys::window().ok_or_else(|| "this page has no window to fetch from".to_string())?;
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    init.set_mode(web_sys::RequestMode::Cors);
    init.set_credentials(web_sys::RequestCredentials::Omit);
    if let Some(b) = body {
        init.set_body(&js_sys::Uint8Array::from(&b[..]));
    }
    let abort = web_sys::AbortController::new().ok();
    if let Some(a) = &abort {
        init.set_signal(Some(&a.signal()));
    }
    let req = web_sys::Request::new_with_str_and_init(url, &init)
        .map_err(|e| format!("could not build the request: {}", js_text(&e)))?;
    for (k, v) in headers {
        req.headers()
            .set(k, v)
            .map_err(|e| format!("the browser will not send the header \"{k}\": {}", js_text(&e)))?;
    }
    if let Some(a) = abort.clone() {
        let fire = wasm_bindgen::closure::Closure::once_into_js(move || a.abort());
        let ms = (timeout_s * 1000.0).clamp(1.0, i32::MAX as f64) as i32;
        let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(fire.unchecked_ref(), ms);
    }
    let timed_out = || abort.as_ref().is_some_and(|a| a.signal().aborted());
    let resp: web_sys::Response = wasm_bindgen_futures::JsFuture::from(win.fetch_with_request(&req))
        .await
        .map_err(|e| {
            if timed_out() {
                format!("no reply within {timeout_s} seconds")
            } else {
                // A CORS refusal and an unreachable host look the same from
                // here, and the first is by far the likelier from a page.
                format!(
                    "could not reach {url} — the server may not allow requests from this page's \
                     address (CORS), or it could not be reached ({})",
                    js_text(&e)
                )
            }
        })?
        .dyn_into()
        .map_err(|_| "the browser answered with something that was not a reply".to_string())?;
    let status = resp.status();
    let content_type = resp.headers().get("content-type").ok().flatten();
    let declared = resp
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|n| n.trim().parse::<usize>().ok());
    if declared.is_some_and(|n| n > max_body) {
        return Err(format!("the reply is larger than the {max_body} byte limit"));
    }
    let buf = wasm_bindgen_futures::JsFuture::from(
        resp.array_buffer().map_err(|e| format!("could not read the reply: {}", js_text(&e)))?,
    )
    .await
    .map_err(|e| if timed_out() { format!("no reply within {timeout_s} seconds") } else { format!("could not read the reply: {}", js_text(&e)) })?;
    let body = js_sys::Uint8Array::new(&buf).to_vec();
    if body.len() > max_body {
        return Err(format!("the reply is larger than the {max_body} byte limit"));
    }
    Ok(FetchReply { status, body, content_type })
}
