//! HTTP control API
//!
//! Lets external clients read and change volume, mute and solo. It runs on
//! its own thread and acts on the same mixer core as the TUI.
//!
//! Every request needs the key the instance was started with, sent as
//! `Authorization: Bearer <key>`. Traffic is plain HTTP, so the key can be read
//! on the network, which is why the API listens on loopback unless told
//! otherwise.
//!
//! The thread must not log: anything written to stderr would garble the TUI.
//! Clients learn about failures from the response instead.

use std::fmt;
use std::hint::black_box;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Request, Response, Server};

use crate::ipc::{ChannelKind, ChannelState};
use crate::mixer::{ChannelUpdate, CoreError, SharedCore, VolumeChange};

/// Address used when the API is enabled without one
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7700";

/// Largest accepted request body. Valid requests are far smaller.
const MAX_BODY_BYTES: u64 = 64 * 1024;

/// Pause after a failed accept, as the failure may be repeating
const RECV_ERROR_DELAY: Duration = Duration::from_millis(100);

/// Parse a listen address: `IP:PORT`, or `PORT` to listen on loopback.
pub fn parse_listen_addr(spec: &str) -> Result<SocketAddr> {
    if let Ok(port) = spec.parse::<u16>() {
        return Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    }
    spec.parse().map_err(|_| {
        anyhow!(
            "Invalid API listen address '{}': expected IP:PORT or PORT, for example {}",
            spec,
            DEFAULT_LISTEN
        )
    })
}

/// The key clients must present. It only ever comes from the command line,
/// and has no `Display` or `Serialize` so that it cannot end up in a message,
/// a response or the config file by accident.
#[derive(Clone)]
pub struct ApiKey(String);

impl From<String> for ApiKey {
    fn from(key: String) -> Self {
        Self(key)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl ApiKey {
    /// Whether the value of an `Authorization` header carries this key.
    fn accepts(&self, authorization: &str) -> bool {
        match authorization.split_once(' ') {
            Some((scheme, token)) if scheme.eq_ignore_ascii_case("Bearer") => {
                constant_time_eq(self.0.as_bytes(), token.as_bytes())
            }
            _ => false,
        }
    }
}

/// Compare a secret with a presented value. The time taken depends only on the
/// length of the presented value, never on where the two differ. Cycling over
/// the secret avoids a branch at its end; the length check rejects what that
/// would otherwise let match.
fn constant_time_eq(secret: &[u8], presented: &[u8]) -> bool {
    let mut difference = secret.len() ^ presented.len();
    for (a, b) in secret.iter().cycle().zip(presented) {
        // Keeps the optimiser from turning this into an early exit
        difference = black_box(difference | usize::from(a ^ b));
    }
    difference == 0
}

/// Check the API settings for this run, given the resolved listen address.
/// Returns what the API needs to start, or `None` if it is disabled.
pub fn check_api_settings(
    listen: Option<&str>,
    key: Option<ApiKey>,
) -> Result<Option<(&str, ApiKey)>> {
    match (listen, key) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(anyhow!(
            "--api-key was given but the API is not enabled: use --api-listen, \
             or api_listen in the config file"
        )),
        (Some(_), None) => Err(anyhow!("The API is enabled but has no key: use --api-key")),
        (Some(_), Some(key)) if key.0.is_empty() => Err(anyhow!("--api-key must not be empty")),
        (Some(listen), Some(key)) => Ok(Some((listen, key))),
    }
}

/// A bound API server that is not serving yet
pub struct ApiServer {
    server: Server,
    key: ApiKey,
}

impl ApiServer {
    /// Bind the listen address, so that a failure can be reported at startup.
    pub fn bind(addr: SocketAddr, key: ApiKey) -> Result<Self> {
        let server = Server::http(addr)
            .map_err(|e| anyhow!("{}", e))
            .with_context(|| format!("Failed to start API server on {}", addr))?;

        if addr.ip().is_loopback() {
            log::info!("API server listening on {}", addr);
        } else {
            // Written before the TUI takes over the terminal
            eprintln!(
                "Warning: API server listening on {} over plain HTTP: \
                 the API key and all traffic travel unencrypted",
                addr
            );
        }
        Ok(Self { server, key })
    }

    /// Serve requests on a dedicated thread until the process exits. It is
    /// not joined on exit: the core refuses changes once it has been closed,
    /// so a request still in flight cannot be lost silently, and a stalled
    /// client cannot hold up quitting.
    pub fn spawn(self, core: SharedCore) -> Result<()> {
        thread::Builder::new()
            .name("rmixer-api".to_string())
            .spawn(move || serve(&self.server, &core, &self.key))
            .context("Failed to start API thread")?;
        Ok(())
    }
}

fn serve(server: &Server, core: &SharedCore, key: &ApiKey) {
    loop {
        match server.recv() {
            Ok(request) if authorized(key, request.headers()) => respond(request, core),
            Ok(request) => reject(request),
            Err(_) => thread::sleep(RECV_ERROR_DELAY),
        }
    }
}

/// Whether the headers of a request carry the key.
fn authorized(key: &ApiKey, headers: &[Header]) -> bool {
    headers
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .is_some_and(|header| key.accepts(header.value.as_str()))
}

/// Refuse a request without reading its body or looking at its route.
///
/// This must stay off the API thread. tiny_http discards the unread part of a
/// body when a request is dropped, which `Request::respond` does after sending
/// the response, so a client that declares a large body and never sends it
/// would otherwise hold the thread indefinitely. The thread ends when the
/// client has sent the body or hung up, and has no access to the core.
fn reject(request: Request) {
    // If no thread can be started, dropping the request answers it with a 500
    let _ = thread::Builder::new()
        .name("rmixer-api-reject".to_string())
        .spawn(move || send(request, ApiResponse::unauthorized()));
}

/// Read a request, handle it and send the response.
fn respond(mut request: Request, core: &SharedCore) {
    let method = request.method().to_string();
    let url = request.url().to_string();

    let mut body = Vec::new();
    let mut limited = request.as_reader().take(MAX_BODY_BYTES + 1);
    let result = match limited.read_to_end(&mut body) {
        Ok(_) if body.len() as u64 > MAX_BODY_BYTES => {
            ApiResponse::error(413, "request body is too large")
        }
        Ok(_) => handle(core, &method, &url, &body),
        Err(_) => ApiResponse::error(400, "failed to read request body"),
    };
    send(request, result);
}

/// Send the response to a request.
fn send(request: Request, result: ApiResponse) {
    let content_type =
        Header::from_bytes("Content-Type", "application/json").expect("static header is valid");
    let mut response = Response::from_string(result.body)
        .with_status_code(result.status)
        .with_header(content_type);
    if let Some(allow) = result.allow {
        response.add_header(Header::from_bytes("Allow", allow).expect("static header is valid"));
    }
    if let Some(challenge) = result.challenge {
        response.add_header(
            Header::from_bytes("WWW-Authenticate", challenge).expect("static header is valid"),
        );
    }

    // A client that hung up does not need to hear about it
    let _ = request.respond(response);
}

/// The outcome of a request
#[derive(Debug)]
struct ApiResponse {
    status: u16,
    body: String,

    /// Value for the `Allow` header of a 405 response
    allow: Option<&'static str>,

    /// Value for the `WWW-Authenticate` header of a 401 response
    challenge: Option<&'static str>,
}

impl ApiResponse {
    fn json<T: Serialize>(value: &T) -> Self {
        match serde_json::to_string(value) {
            Ok(body) => Self {
                status: 200,
                body,
                allow: None,
                challenge: None,
            },
            Err(_) => Self::error(500, "failed to serialize response"),
        }
    }

    fn error(status: u16, message: &str) -> Self {
        let body = serde_json::json!({ "error": message }).to_string();
        Self {
            status,
            body,
            allow: None,
            challenge: None,
        }
    }

    /// The same for every refused request, so that it does not tell a client
    /// whether its header was missing or wrong.
    fn unauthorized() -> Self {
        Self {
            challenge: Some("Bearer"),
            ..Self::error(401, "unauthorized")
        }
    }

    fn method_not_allowed(allow: &'static str) -> Self {
        Self {
            allow: Some(allow),
            ..Self::error(405, "method not allowed")
        }
    }
}

/// One channel, as clients see it
#[derive(Serialize)]
struct ChannelView<'a> {
    name: &'a str,
    port_count: usize,
    volume_db: f32,
    muted: bool,

    /// Only inputs have solo
    #[serde(skip_serializing_if = "Option::is_none")]
    soloed: Option<bool>,
}

impl<'a> ChannelView<'a> {
    fn new(kind: ChannelKind, channel: &'a ChannelState) -> Self {
        Self {
            name: &channel.name,
            port_count: channel.port_count,
            volume_db: channel.volume_db,
            muted: channel.muted,
            soloed: (kind == ChannelKind::Input).then_some(channel.soloed),
        }
    }
}

/// All channels, as clients see them
#[derive(Serialize)]
struct StateView<'a> {
    inputs: Vec<ChannelView<'a>>,
    outputs: Vec<ChannelView<'a>>,
}

/// Body of a PATCH request. Unknown fields are rejected, so that a misspelt
/// setting is an error rather than a request that silently does nothing.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    volume_db: Option<f32>,
    volume_delta_db: Option<f32>,
    muted: Option<bool>,
    soloed: Option<bool>,
}

impl TryFrom<PatchBody> for ChannelUpdate {
    type Error = &'static str;

    fn try_from(body: PatchBody) -> Result<Self, Self::Error> {
        let volume = match (body.volume_db, body.volume_delta_db) {
            (Some(_), Some(_)) => {
                return Err("volume_db and volume_delta_db cannot be combined");
            }
            (Some(db), None) => Some(VolumeChange::Absolute(db)),
            (None, Some(delta)) => Some(VolumeChange::Delta(delta)),
            (None, None) => None,
        };
        let update = ChannelUpdate {
            volume,
            muted: body.muted,
            soloed: body.soloed,
        };
        if update == ChannelUpdate::default() {
            return Err("request does not change anything");
        }
        Ok(update)
    }
}

/// Route a request that has passed authentication. Kept free of HTTP server
/// types so it can be tested alone.
///
/// OPTIONS is deliberately not answered: a web page making a cross-origin
/// PATCH needs a successful preflight first, so this keeps browsers from
/// changing the mixer on behalf of an arbitrary site.
fn handle(core: &SharedCore, method: &str, url: &str, body: &[u8]) -> ApiResponse {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let Some(route) = path.strip_prefix("/api/v1/") else {
        return ApiResponse::error(404, "not found");
    };
    let segments: Vec<&str> = route.split('/').collect();

    match segments[..] {
        ["state"] => match method {
            "GET" => get_state(core),
            _ => ApiResponse::method_not_allowed("GET"),
        },
        ["inputs", name] => patch_channel(core, method, ChannelKind::Input, name, body),
        ["outputs", name] => patch_channel(core, method, ChannelKind::Output, name, body),
        _ => ApiResponse::error(404, "not found"),
    }
}

fn get_state(core: &SharedCore) -> ApiResponse {
    let core = core.lock();
    let view = |kind| {
        core.state()
            .channels(kind)
            .iter()
            .map(|channel| ChannelView::new(kind, channel))
            .collect()
    };
    ApiResponse::json(&StateView {
        inputs: view(ChannelKind::Input),
        outputs: view(ChannelKind::Output),
    })
}

fn patch_channel(
    core: &SharedCore,
    method: &str,
    kind: ChannelKind,
    encoded_name: &str,
    body: &[u8],
) -> ApiResponse {
    if method != "PATCH" {
        return ApiResponse::method_not_allowed("PATCH");
    }
    let Some(name) = percent_decode(encoded_name) else {
        return ApiResponse::error(400, "malformed channel name");
    };

    // The lock is held from the lookup to the response, so the response shows
    // what this request applied and not a later change by the TUI.
    let mut core = core.lock();
    let Some(index) = core.find_channel(kind, &name) else {
        return ApiResponse::error(404, "unknown channel");
    };

    let update = match serde_json::from_slice::<PatchBody>(body) {
        Ok(body) => ChannelUpdate::try_from(body),
        Err(e) => return ApiResponse::error(400, &format!("invalid request body: {}", e)),
    };
    let update = match update {
        Ok(update) => update,
        Err(message) => return ApiResponse::error(400, message),
    };

    match core.update(kind, index, update) {
        Ok(()) => ApiResponse::json(&ChannelView::new(kind, &core.state().channels(kind)[index])),
        Err(e) => ApiResponse::error(status_for(e), &e.to_string()),
    }
}

fn status_for(error: CoreError) -> u16 {
    match error {
        CoreError::QueueFull | CoreError::Closed => 503,
        CoreError::UnknownChannel => 404,
        CoreError::SoloOnOutput | CoreError::InvalidVolume => 400,
    }
}

/// Decode a percent-encoded path segment. Returns `None` if the encoding is
/// malformed or the result is not UTF-8.
fn percent_decode(segment: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(segment.len());
    let mut bytes = segment.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::ControlMsg;
    use crate::mixer::tests::{test_core, test_state};
    use crate::mixer::MixerCore;
    use rtrb::{Consumer, RingBuffer};
    use serde_json::{json, Value};
    use std::io::Write;
    use std::net::TcpStream;

    const TEST_KEY: &str = "test-key";

    fn test_key() -> ApiKey {
        ApiKey::from(TEST_KEY.to_string())
    }

    fn shared_core(capacity: usize) -> (SharedCore, Consumer<ControlMsg>) {
        let (core, consumer) = test_core(capacity);
        (SharedCore::new(core), consumer)
    }

    fn patch(core: &SharedCore, url: &str, body: &str) -> (u16, Value) {
        let response = handle(core, "PATCH", url, body.as_bytes());
        (
            response.status,
            serde_json::from_str(&response.body).unwrap(),
        )
    }

    fn state(core: &SharedCore) -> Value {
        let response = handle(core, "GET", "/api/v1/state", b"");
        assert_eq!(response.status, 200);
        serde_json::from_str(&response.body).unwrap()
    }

    #[test]
    fn default_listen_address_is_loopback() {
        assert!(parse_listen_addr(DEFAULT_LISTEN)
            .unwrap()
            .ip()
            .is_loopback());
    }

    #[test]
    fn bare_port_listens_on_loopback() {
        let addr = parse_listen_addr("9000").unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 9000);
    }

    #[test]
    fn other_addresses_must_be_spelled_out() {
        let addr = parse_listen_addr("0.0.0.0:9000").unwrap();
        assert!(addr.ip().is_unspecified());
        assert!(parse_listen_addr("[::1]:9000").unwrap().ip().is_loopback());
    }

    #[test]
    fn invalid_listen_addresses_are_rejected() {
        for spec in ["", "example", "127.0.0.1", "example:9000", "99999"] {
            let error = parse_listen_addr(spec).unwrap_err();
            assert!(error.to_string().contains("Invalid API listen address"));
        }
    }

    #[test]
    fn state_lists_all_channels() {
        let (core, _consumer) = shared_core(8);
        assert_eq!(
            state(&core),
            json!({
                "inputs": [
                    {"name": "Mic", "port_count": 1, "volume_db": -6.0,
                     "muted": false, "soloed": false},
                    {"name": "Music", "port_count": 2, "volume_db": 0.0,
                     "muted": false, "soloed": false},
                ],
                "outputs": [
                    {"name": "Main", "port_count": 2, "volume_db": 0.0, "muted": false},
                    {"name": "Monitor", "port_count": 2, "volume_db": 0.0, "muted": false},
                ],
            })
        );
    }

    #[test]
    fn patch_sets_volume_and_returns_the_channel() {
        let (core, mut consumer) = shared_core(8);
        let (status, body) = patch(&core, "/api/v1/inputs/Mic", r#"{"volume_db": -12.5}"#);
        assert_eq!(status, 200);
        assert_eq!(
            body,
            json!({"name": "Mic", "port_count": 1, "volume_db": -12.5,
                   "muted": false, "soloed": false})
        );
        assert_eq!(core.lock().state().inputs[0].volume_db, -12.5);
        assert!(matches!(
            consumer.pop(),
            Ok(ControlMsg::SetInputVolume { channel: 0, .. })
        ));
    }

    #[test]
    fn patch_returns_the_clamped_volume() {
        let (core, _consumer) = shared_core(8);
        let (status, body) = patch(&core, "/api/v1/outputs/Main", r#"{"volume_db": 40}"#);
        assert_eq!(status, 200);
        assert_eq!(body["volume_db"], json!(12.0));
        let (_, body) = patch(&core, "/api/v1/outputs/Main", r#"{"volume_db": -100}"#);
        assert_eq!(body["volume_db"], json!(-60.0));
    }

    #[test]
    fn patch_applies_a_volume_delta() {
        let (core, _consumer) = shared_core(8);
        let url = "/api/v1/inputs/Mic";
        let (_, body) = patch(&core, url, r#"{"volume_delta_db": 1.5}"#);
        assert_eq!(body["volume_db"], json!(-4.5));
        let (_, body) = patch(&core, url, r#"{"volume_delta_db": -100}"#);
        assert_eq!(body["volume_db"], json!(-60.0));
    }

    #[test]
    fn patch_sets_mute_and_solo_absolutely() {
        let (core, _consumer) = shared_core(8);
        let body = r#"{"muted": true, "soloed": true}"#;
        for _ in 0..2 {
            let (status, response) = patch(&core, "/api/v1/inputs/Music", body);
            assert_eq!(status, 200);
            assert_eq!(response["muted"], json!(true));
            assert_eq!(response["soloed"], json!(true));
        }
        let (status, response) = patch(&core, "/api/v1/outputs/Main", r#"{"muted": true}"#);
        assert_eq!(status, 200);
        assert_eq!(response["muted"], json!(true));
        assert!(response.get("soloed").is_none());
    }

    #[test]
    fn solo_on_an_output_is_rejected() {
        let (core, _consumer) = shared_core(8);
        let before = state(&core);
        for body in [r#"{"soloed": true}"#, r#"{"soloed": false, "muted": true}"#] {
            let (status, response) = patch(&core, "/api/v1/outputs/Main", body);
            assert_eq!(status, 400);
            assert!(response["error"].is_string());
        }
        assert_eq!(state(&core), before);
    }

    #[test]
    fn unknown_channels_return_404() {
        let (core, _consumer) = shared_core(8);
        for url in [
            "/api/v1/inputs/Nope",
            "/api/v1/outputs/Mic",
            "/api/v1/inputs/",
        ] {
            let (status, _) = patch(&core, url, r#"{"muted": true}"#);
            assert_eq!(status, 404, "{url}");
        }
    }

    #[test]
    fn full_queue_returns_an_error_and_leaves_the_state_unchanged() {
        let (core, mut consumer) = shared_core(2);
        let url = "/api/v1/inputs/Mic";

        // Two settings fit, three do not: nothing of the request may apply
        let (status, _) = patch(&core, url, r#"{"volume_db": 1, "muted": true}"#);
        assert_eq!(status, 200);
        let before = state(&core);
        let (status, response) = patch(&core, url, r#"{"volume_db": 2}"#);
        assert_eq!(status, 503);
        assert!(response["error"].as_str().unwrap().contains("queue"));
        assert_eq!(state(&core), before);

        // The request works again once the audio thread has drained the queue
        while consumer.pop().is_ok() {}
        let (status, response) = patch(&core, url, r#"{"volume_db": 2}"#);
        assert_eq!(status, 200);
        assert_eq!(response["volume_db"], json!(2.0));
    }

    #[test]
    fn closed_core_returns_an_error() {
        let (core, _consumer) = shared_core(8);
        core.lock().close();
        let (status, _) = patch(&core, "/api/v1/inputs/Mic", r#"{"muted": true}"#);
        assert_eq!(status, 503);
        assert_eq!(state(&core)["inputs"][0]["muted"], json!(false));
    }

    #[test]
    fn invalid_bodies_are_rejected() {
        let (core, _consumer) = shared_core(8);
        let before = state(&core);
        for body in [
            "",
            "not json",
            "[]",
            "{}",
            r#"{"volume_db": null}"#,
            r#"{"volume_db": "loud"}"#,
            r#"{"volume_db": 1e300}"#,
            r#"{"volume_delta_db": 1e300}"#,
            r#"{"muted": "yes"}"#,
            r#"{"mute": true}"#,
            r#"{"volume_db": 1, "volume_delta_db": 1}"#,
        ] {
            let (status, response) = patch(&core, "/api/v1/inputs/Mic", body);
            assert_eq!(status, 400, "{body}");
            assert!(response["error"].is_string(), "{body}");
        }
        assert_eq!(state(&core), before);
    }

    #[test]
    fn channel_names_are_percent_decoded() {
        let (producer, _consumer) = RingBuffer::new(8);
        let mut mixer_state = test_state();
        mixer_state.inputs[0].name = "Lead Vocal/1 é".to_string();
        let core = SharedCore::new(MixerCore::new(mixer_state, producer));

        let url = "/api/v1/inputs/Lead%20Vocal%2F1%20%C3%A9";
        let (status, body) = patch(&core, url, r#"{"muted": true}"#);
        assert_eq!(status, 200);
        assert_eq!(body["name"], json!("Lead Vocal/1 é"));
    }

    #[test]
    fn malformed_channel_names_are_rejected() {
        let (core, _consumer) = shared_core(8);
        for url in [
            "/api/v1/inputs/%",
            "/api/v1/inputs/%4",
            "/api/v1/inputs/%zz",
            "/api/v1/inputs/%ff",
        ] {
            let (status, _) = patch(&core, url, r#"{"muted": true}"#);
            assert_eq!(status, 400, "{url}");
        }
    }

    #[test]
    fn query_strings_are_ignored() {
        let (core, _consumer) = shared_core(8);
        assert_eq!(handle(&core, "GET", "/api/v1/state?x=1", b"").status, 200);
    }

    #[test]
    fn unknown_paths_return_404() {
        let (core, _consumer) = shared_core(8);
        for url in [
            "/",
            "/api/v1/",
            "/api/v1/nope",
            "/api/v2/state",
            "/api/v1/state/extra",
            "/api/v1/inputs",
            "/api/v1/inputs/Mic/extra",
        ] {
            assert_eq!(handle(&core, "GET", url, b"").status, 404, "{url}");
        }
    }

    #[test]
    fn wrong_methods_return_405() {
        let (core, _consumer) = shared_core(8);
        let response = handle(&core, "PATCH", "/api/v1/state", b"{}");
        assert_eq!((response.status, response.allow), (405, Some("GET")));
        let response = handle(&core, "GET", "/api/v1/inputs/Mic", b"");
        assert_eq!((response.status, response.allow), (405, Some("PATCH")));
    }

    #[test]
    fn cors_preflight_is_refused() {
        let (core, _consumer) = shared_core(8);
        assert_eq!(
            handle(&core, "OPTIONS", "/api/v1/inputs/Mic", b"").status,
            405
        );
    }

    #[test]
    fn missing_header_is_refused() {
        assert!(!authorized(&test_key(), &[]));
        let other = Header::from_bytes("Accept", "*/*").unwrap();
        assert!(!authorized(&test_key(), &[other]));
    }

    #[test]
    fn header_name_is_case_insensitive() {
        for name in ["Authorization", "authorization", "AUTHORIZATION"] {
            let header = Header::from_bytes(name, format!("Bearer {TEST_KEY}")).unwrap();
            assert!(authorized(&test_key(), &[header]), "{name}");
        }
    }

    #[test]
    fn right_key_is_accepted() {
        assert!(test_key().accepts("Bearer test-key"));
    }

    #[test]
    fn scheme_is_case_insensitive() {
        for scheme in ["bearer", "BEARER", "bEaReR"] {
            assert!(
                test_key().accepts(&format!("{scheme} {TEST_KEY}")),
                "{scheme}"
            );
        }
    }

    #[test]
    fn wrong_key_is_refused() {
        for value in [
            "Bearer nope",
            "Bearer TEST-KEY",
            "Bearer test-kez",
            "Bearer uest-key",
        ] {
            assert!(!test_key().accepts(value), "{value}");
        }
    }

    #[test]
    fn wrong_scheme_is_refused() {
        for value in [
            "Basic test-key",
            "Token test-key",
            "Bearertest-key",
            "test-key",
            " test-key",
            "Bearer",
            "",
        ] {
            assert!(!test_key().accepts(value), "{value:?}");
        }
    }

    #[test]
    fn prefixes_of_the_key_and_keys_it_prefixes_are_refused() {
        for value in ["Bearer ", "Bearer t", "Bearer test-ke", "Bearer test-key2"] {
            assert!(!test_key().accepts(value), "{value}");
        }
        // The other way round: the real key is the shorter or the longer one
        let long = ApiKey::from("test-key-long".to_string());
        assert!(!long.accepts("Bearer test-key"));
        let short = ApiKey::from("test".to_string());
        assert!(!short.accepts("Bearer test-key"));
    }

    #[test]
    fn repeating_the_key_is_refused() {
        assert!(!test_key().accepts("Bearer test-keytest-key"));
        assert!(!constant_time_eq(b"ab", b"abab"));
    }

    #[test]
    fn comparison_checks_every_position() {
        assert!(constant_time_eq(b"abc", b"abc"));
        for presented in [&b"xbc"[..], b"axc", b"abx", b"ab", b"abcd", b""] {
            assert!(!constant_time_eq(b"abc", presented));
        }
    }

    #[test]
    fn key_is_hidden_from_debug_output() {
        assert!(!format!("{:?}", Some(test_key())).contains(TEST_KEY));
    }

    #[test]
    fn disabled_api_without_a_key_is_fine() {
        assert!(check_api_settings(None, None).unwrap().is_none());
    }

    #[test]
    fn enabled_api_with_a_key_is_fine() {
        let (listen, key) = check_api_settings(Some(DEFAULT_LISTEN), Some(test_key()))
            .unwrap()
            .unwrap();
        assert_eq!(listen, DEFAULT_LISTEN);
        assert!(key.accepts("Bearer test-key"));
    }

    #[test]
    fn enabled_api_without_a_key_is_an_error() {
        for listen in [DEFAULT_LISTEN, "9000", "0.0.0.0:9000"] {
            let error = check_api_settings(Some(listen), None).unwrap_err();
            assert!(error.to_string().contains("--api-key"), "{listen}");
        }
    }

    #[test]
    fn empty_key_is_an_error() {
        let error = check_api_settings(Some(DEFAULT_LISTEN), Some(ApiKey::from(String::new())))
            .unwrap_err();
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn key_without_the_api_is_an_error() {
        let error = check_api_settings(None, Some(test_key())).unwrap_err();
        assert!(error.to_string().contains("not enabled"));
        assert!(!error.to_string().contains(TEST_KEY));
    }

    /// A parsed HTTP response
    struct HttpResponse {
        status_line: String,
        head: String,
        body: String,
    }

    impl HttpResponse {
        fn is_refusal(&self) -> bool {
            self.status_line.contains("401")
                && self
                    .head
                    .to_ascii_lowercase()
                    .contains("www-authenticate: bearer\r\n")
                && self.body == r#"{"error":"unauthorized"}"#
        }
    }

    /// Read one response, leaving the connection open. A stalled server fails
    /// the read rather than hanging the test.
    fn read_response(stream: &mut TcpStream) -> HttpResponse {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 1024];
        let split = loop {
            if let Some(i) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "connection closed before the response");
            received.extend_from_slice(&buffer[..count]);
        };
        let head = String::from_utf8(received[..split + 2].to_vec()).unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")?
                    .parse()
                    .ok()
            })
            .unwrap_or(0);
        while received.len() < split + 4 + length {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "connection closed before the end of the body");
            received.extend_from_slice(&buffer[..count]);
        }
        HttpResponse {
            status_line: head.lines().next().unwrap().to_string(),
            body: String::from_utf8(received[split + 4..].to_vec()).unwrap(),
            head,
        }
    }

    /// Send a raw HTTP request and return the response.
    fn http_exchange(addr: SocketAddr, request: &str) -> HttpResponse {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        read_response(&mut stream)
    }

    /// Send a raw HTTP request and return the status line and the body.
    fn http_request(addr: SocketAddr, request: &str) -> (String, String) {
        let response = http_exchange(addr, request);
        (response.status_line, response.body)
    }

    /// A request with a body, with `auth` being complete header lines.
    fn request_with_body(method: &str, path: &str, auth: &str, body: &str) -> String {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}\
             Content-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len(),
        )
    }

    fn bearer_header() -> String {
        format!("Authorization: Bearer {TEST_KEY}\r\n")
    }

    fn start_server(core: SharedCore) -> SocketAddr {
        let server = ApiServer::bind("127.0.0.1:0".parse().unwrap(), test_key()).unwrap();
        let addr = server.server.server_addr().to_ip().unwrap();
        server.spawn(core).unwrap();
        addr
    }

    #[test]
    fn server_handles_requests_over_a_socket() {
        let (core, _consumer) = shared_core(8);
        let addr = start_server(core.clone());
        let auth = bearer_header();

        let (status, response) = http_request(
            addr,
            &request_with_body(
                "PATCH",
                "/api/v1/inputs/Mic",
                &auth,
                r#"{"volume_db": -3.0}"#,
            ),
        );
        assert!(status.contains("200"), "{status}");
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["volume_db"], json!(-3.0));
        assert_eq!(core.lock().state().inputs[0].volume_db, -3.0);

        let (status, response) = http_request(
            addr,
            &format!(
                "GET /api/v1/state HTTP/1.1\r\nHost: localhost\r\n{auth}Connection: close\r\n\r\n"
            ),
        );
        assert!(status.contains("200"), "{status}");
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["inputs"][0]["volume_db"], json!(-3.0));
    }

    #[test]
    fn every_route_requires_the_key() {
        let (core, _consumer) = shared_core(8);
        let addr = start_server(core);

        for (method, path) in [
            ("GET", "/api/v1/state"),
            ("PATCH", "/api/v1/inputs/Mic"),
            ("PATCH", "/api/v1/outputs/Main"),
            ("PATCH", "/api/v1/inputs/Nope"),
            ("GET", "/api/v1/inputs/Mic"),
            ("POST", "/api/v1/state"),
            ("DELETE", "/api/v1/state"),
            ("OPTIONS", "/api/v1/state"),
            ("OPTIONS", "/api/v1/inputs/Mic"),
            ("GET", "/"),
            ("GET", "/api/v2/state"),
            // The key does not work as a query parameter
            ("GET", "/api/v1/state?api_key=test-key"),
            ("GET", "/api/v1/state?access_token=test-key"),
        ] {
            let body = r#"{"muted": true}"#;
            for auth in ["", "Authorization: Bearer wrong-key\r\n"] {
                let response = http_exchange(addr, &request_with_body(method, path, auth, body));
                assert!(response.is_refusal(), "{method} {path}: {}", response.head);
                assert!(!response.body.contains("wrong-key"));
                assert!(!response.head.contains("wrong-key"));
            }
        }
    }

    #[test]
    fn key_reaches_the_routes() {
        let (core, _consumer) = shared_core(8);
        let addr = start_server(core);
        let auth = bearer_header();

        for (method, path, status) in [
            ("GET", "/api/v1/state", "200"),
            ("PATCH", "/api/v1/inputs/Nope", "404"),
            ("GET", "/", "404"),
            ("OPTIONS", "/api/v1/inputs/Mic", "405"),
        ] {
            let response = http_exchange(addr, &request_with_body(method, path, &auth, ""));
            assert!(response.status_line.contains(status), "{method} {path}");
            assert!(!response
                .head
                .to_ascii_lowercase()
                .contains("www-authenticate"));
        }
    }

    #[test]
    fn rejected_patch_leaves_the_state_unchanged() {
        let (core, mut consumer) = shared_core(8);
        let addr = start_server(core.clone());
        let before = state(&core);
        let body = r#"{"volume_db": -20.0, "muted": true}"#;
        let path = "/api/v1/inputs/Mic";

        for auth in [
            "",
            "Authorization: Bearer wrong-key\r\n",
            "Authorization: Basic test-key\r\n",
        ] {
            let response = http_exchange(addr, &request_with_body("PATCH", path, auth, body));
            assert!(response.is_refusal(), "{auth}");
        }
        assert_eq!(state(&core), before);
        assert!(consumer.pop().is_err());

        let auth = "authorization: bearer test-key\r\n";
        let response = http_exchange(addr, &request_with_body("PATCH", path, auth, body));
        assert!(response.status_line.contains("200"), "{}", response.head);
        assert_eq!(core.lock().state().inputs[0].volume_db, -20.0);
        assert!(core.lock().state().inputs[0].muted);
        assert!(consumer.pop().is_ok());
    }

    #[test]
    fn large_unauthenticated_body_is_not_waited_for() {
        let (core, _consumer) = shared_core(8);
        let addr = start_server(core);

        // Streams that stay open, with their bodies never sent
        let mut stalled = Vec::new();
        for declared in [
            "Content-Length: 1000000\r\n",
            "Content-Length: 1000000\r\nExpect: 100-continue\r\n",
            "Transfer-Encoding: chunked\r\n",
        ] {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .write_all(
                    format!(
                        "PATCH /api/v1/inputs/Mic HTTP/1.1\r\nHost: localhost\r\n{declared}\r\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
            // Not 100 Continue, which would invite the body
            let response = read_response(&mut stream);
            assert!(response.is_refusal(), "{declared}: {}", response.head);
            stalled.push(stream);
        }

        // The API thread is not held by any of them
        let request = format!(
            "GET /api/v1/state HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
            bearer_header()
        );
        let (status, _) = http_request(addr, &request);
        assert!(status.contains("200"), "{status}");
        let request = "GET /api/v1/state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        assert!(http_exchange(addr, request).is_refusal());
    }

    #[test]
    fn server_rejects_oversized_bodies() {
        let (core, _consumer) = shared_core(8);
        let addr = start_server(core);

        let body = " ".repeat(MAX_BODY_BYTES as usize + 1);
        let (status, _) = http_request(
            addr,
            &request_with_body("PATCH", "/api/v1/inputs/Mic", &bearer_header(), &body),
        );
        assert!(status.contains("413"), "{status}");
    }

    #[test]
    fn binding_a_port_in_use_is_a_clear_error() {
        let first = ApiServer::bind("127.0.0.1:0".parse().unwrap(), test_key()).unwrap();
        let addr = first.server.server_addr().to_ip().unwrap();
        let error = ApiServer::bind(addr, test_key()).err().unwrap();
        assert!(error
            .to_string()
            .contains(&format!("Failed to start API server on {}", addr)));
    }
}
