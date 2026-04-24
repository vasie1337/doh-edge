use worker::*;

pub const DNS_MSG: &str = "application/dns-message";
const UPSTREAM_URL: &str = "https://1.1.1.1/dns-query";

pub fn dns_post_init(query: &[u8]) -> Result<RequestInit> {
    let headers = Headers::new();
    headers.set("accept", DNS_MSG)?;
    headers.set("content-type", DNS_MSG)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(wasm_bindgen::JsValue::from(
            js_sys::Uint8Array::from(query),
        )));
    Ok(init)
}

pub async fn fetch_upstream(query: &[u8]) -> Result<Vec<u8>> {
    let init = dns_post_init(query)?;
    let req = Request::new_with_init(UPSTREAM_URL, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    resp.bytes().await
}

pub fn dns_response(bytes: Vec<u8>) -> Result<Response> {
    let mut resp = Response::from_bytes(bytes)?;
    resp.headers_mut().set("content-type", DNS_MSG)?;
    Ok(resp)
}

pub fn now_ms() -> f64 {
    js_sys::Date::now()
}
