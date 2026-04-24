use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use worker::*;

const UPSTREAM: &str = "https://1.1.1.1/dns-query";
const DNS_MSG: &str = "application/dns-message";

#[event(fetch)]
async fn fetch(mut req: Request, _env: Env, _ctx: Context) -> Result<Response> {
    let body: Vec<u8> = match req.method() {
        Method::Get => {
            let url = req.url()?;
            let Some((_, dns)) = url.query_pairs().find(|(k, _)| k == "dns") else {
                return Response::error("missing ?dns=", 400);
            };
            URL_SAFE_NO_PAD
                .decode(dns.trim_end_matches('=').as_bytes())
                .map_err(|e| Error::RustError(format!("bad base64url: {e}")))?
        }
        Method::Post => req.bytes().await?,
        _ => return Response::error("method not allowed", 405),
    };

    let headers = Headers::new();
    headers.set("accept", DNS_MSG)?;
    headers.set("content-type", DNS_MSG)?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(wasm_bindgen::JsValue::from(
            js_sys::Uint8Array::from(body.as_slice()),
        )));

    let upstream = Request::new_with_init(UPSTREAM, &init)?;
    let mut resp = Fetch::Request(upstream).send().await?;
    let bytes = resp.bytes().await?;

    let mut out = Response::from_bytes(bytes)?;
    out.headers_mut().set("content-type", DNS_MSG)?;
    Ok(out)
}
