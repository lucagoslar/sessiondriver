use async_lock::{Mutex, RwLock};
use axum::body::{Body, to_bytes};
use axum::extract::{FromRef, Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Router, ServiceExt};
use clap::Parser;
use log::{debug, error, info};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::{Stdio, exit};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::signal;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Name of the person to greet
    #[arg(env = "SESSIONDRIVER_PORT", long, default_value_t = 4444)]
    pub port: u16,

    /// Number of times to greet
    #[arg(env = "SESSIONDRIVER_HOST", long, default_value_t = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))]
    pub host: IpAddr,

    /// Location of WebDriver executable
    #[arg(env = "SESSIONDRIVER_WEBDRIVER", long)]
    pub webdriver: Box<Path>,

    /// Time after which a browser is asked to shut down
    #[arg(env = "SESSIONDRIVER_TTI", long, value_parser = parse_duration, default_value_t = WrappedDuration(Duration::from_secs(43200)))]
    pub tti: WrappedDuration,

    /// Additional parameters a WebDriver will be started with
    /// (Do not manually set the port)
    #[arg(env = "SESSIONDRIVER_PARAMETERS", long)]
    pub parameters: Option<String>,

    /// Protocol used to communicate with browsers
    #[arg(env = "SESSIONDRIVER_PROTOCOL", long, default_value_t = String::from("http://"))]
    pub protocol: String,
}

#[derive(Debug, Clone)]
pub struct WrappedDuration(Duration);

impl std::fmt::Display for WrappedDuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.as_secs().to_string())?;
        f.write_str("s")
    }
}

fn parse_duration(s: &str) -> Result<WrappedDuration, String> {
    Ok(WrappedDuration(
        humantime::parse_duration(s).map_err(|e| e.to_string())?,
    ))
}

pub struct Browser {
    pub address: SocketAddr,
    pub process: Mutex<Child>,
    pub cleanup: Mutex<JoinHandle<()>>,
}

pub struct WebDriverMeta {
    pub path: Box<Path>,
    pub parameters: Option<String>,
    pub next_port: Mutex<u16>,
    pub tti: Duration,
    pub host: IpAddr,
    pub protocol: String,
}

type Browsers = Arc<RwLock<HashMap<Uuid, Browser>>>;

#[derive(Clone, FromRef)]
pub struct AppState {
    pub browsers: Browsers,
    pub http: Client,
    pub webdriver: Arc<WebDriverMeta>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = Args::parse();

    let parameters = match args.parameters {
        Some(mut p) => {
            if (p.starts_with("\\\"") && p.ends_with("\\\""))
                || (p.starts_with("\\'") && p.ends_with("\\'"))
            {
                let tmp = &p[2..];
                p = String::from(&tmp[..2]);
            } else if (p.starts_with('"') && p.ends_with('"'))
                || (p.starts_with("'") && p.ends_with("'"))
            {
                let tmp = &p[1..];
                p = String::from(&tmp[..1]);
            }

            Some(p)
        }
        None => None,
    };

    let state = AppState {
        browsers: Arc::new(RwLock::new(HashMap::new())),
        http: Client::new(),
        webdriver: Arc::new(WebDriverMeta {
            path: args.webdriver,
            parameters,
            tti: args.tti.0,
            next_port: Mutex::new(4445),
            host: args.host,
            protocol: args.protocol,
        }),
    };

    let app = Router::default().fallback(proxy).with_state(state);

    let listener = TcpListener::bind((args.host, args.port)).await?;
    info!("Listening on {}:{}", args.host, args.port);

    Ok(
        axum::serve(listener, ServiceExt::<Request>::into_make_service(app))
            .with_graceful_shutdown(graceful_shutdown())
            .await?,
    )
}

pub async fn proxy(
    State(browsers): State<Browsers>,
    State(http): State<Client>,
    State(webdriver_meta): State<Arc<WebDriverMeta>>,
    request: Request,
) -> Result<Response, Response> {
    let path = request.uri().path().trim_end_matches('/');
    info!("{}", path);

    if (request.method() == Method::GET || request.method() == Method::HEAD) && path == "/status" {
        let response = Response::builder()
            .status(200)
            .header("Content-Type", "application/json");
        let body = Body::from(r#"{ "value": { "ready": true, "message": "" } }"#);
        return Ok(response.body(body).map_err(internal_server_error)?);
    }

    if request.method() == Method::POST && path == "/session" {
        let port = loop {
            let mut port = webdriver_meta.next_port.lock().await;
            if let Err(_) = TcpListener::bind((webdriver_meta.host, *port)).await {
                *port = *port + 1;
                continue;
            }
            let usable_port = *port;
            *port = *port + 1;

            break usable_port;
        };

        let mut command = Command::new(webdriver_meta.path.as_ref());
        command.arg(&format!("--port={}", port));
        command.arg(&format!("--host={}", webdriver_meta.host));

        if let Some(parameters) = webdriver_meta.parameters.as_ref() {
            for parameter in parameters.split(' ') {
                command.arg(parameter);
            }
        }

        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        command.kill_on_drop(true);
        debug!("Spawning browser using {:?}", command);

        let child = command.spawn().map_err(internal_server_error)?;
        info!("Browser spawned");

        let socket_address = SocketAddr::new(webdriver_meta.host, port);

        let mut i = 0;
        loop {
            if let Ok(response) = http
                .get(format!(
                    "{}{}/status",
                    webdriver_meta.protocol, socket_address
                ))
                .send()
                .await
            {
                if response.status().is_success() {
                    debug!("Browser started");
                    break;
                }
            }
            i = i + 1;
            sleep(Duration::from_millis(125)).await;

            if i == 40 || i == 80 || i == 120 || i == 480 {
                eprintln!(
                    "There might be an issue with the WebDriver (Please check your configuration)"
                );

                if i == 480 {
                    exit(1);
                }
            }
        }

        let driver_response = proxy_request(
            http,
            &webdriver_meta.protocol,
            socket_address,
            request,
            false,
        )
        .await?;
        debug!("Proxied request");

        let mut response = Response::builder();

        for (key, value) in driver_response.headers() {
            response = response.header(key.as_str(), value.as_ref());
        }

        response = response.status(driver_response.status().as_u16());

        #[derive(Debug, Deserialize, Serialize)]
        struct Value {
            pub value: Session,
        }

        #[derive(Debug, Deserialize, Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Session {
            pub session_id: Option<Uuid>,
            pub capabilities: serde_json::Value,
        }

        let body = driver_response
            .bytes()
            .await
            .map_err(internal_server_error)?;
        debug!("Collected body {:?}", String::from_utf8_lossy(&body));
        let mut body: Value = serde_json::from_slice(&body).map_err(internal_server_error)?;
        debug!("Deserialised body");
        let session_id = body.value.session_id.unwrap_or(Uuid::default());
        body.value.session_id = Some(session_id);
        debug!("Extracted session {:?}", session_id);
        let _browsers = browsers.clone();
        browsers.write().await.insert(
            session_id,
            Browser {
                address: socket_address,
                process: Mutex::new(child),
                cleanup: Mutex::new(tokio::spawn(async move {
                    sleep(webdriver_meta.tti).await;
                    _browsers.write().await.remove(&session_id);
                    info!("Removed {:?}", session_id);
                })),
            },
        );

        let body = Body::from(serde_json::to_string(&body).expect("String to JSON from JSON"));
        return Ok(response.body(body).map_err(internal_server_error)?);
    }

    let mut uuid = path.trim_start_matches("/session/");
    if let Some(i) = uuid.find('/') {
        uuid = &uuid[..i];
    }
    let uuid = uuid.parse::<Uuid>().map_err(bad_request_error)?;

    if request.method() == Method::DELETE && path == format!("/session/{}", uuid) {
        if let Some(browser) = browsers.write().await.remove(&uuid) {
            info!("Removed {:?}", uuid);
            browser.cleanup.lock().await.abort();
            let driver_response = proxy_request(
                http,
                &webdriver_meta.protocol,
                browser.address,
                request,
                false,
            )
            .await?;

            let mut response = Response::builder();
            for (key, value) in driver_response.headers() {
                response = response.header(key.as_str(), value.as_ref());
            }
            response = response.status(driver_response.status().as_u16());
            let body = Body::from(
                driver_response
                    .bytes()
                    .await
                    .map_err(internal_server_error)?,
            );
            return Ok(response.body(body).map_err(internal_server_error)?);
        }
    }

    let _browsers = browsers.clone();
    let browsers = browsers.read().await;
    let browser = match browsers.get(&uuid) {
        Some(browser) => browser,
        None => {
            debug!("{:?} not found", uuid);
            return Err((StatusCode::NOT_FOUND, Body::empty()).into_response());
        }
    };

    {
        let mut cleanup = browser.cleanup.lock().await;
        cleanup.abort();
        let tti = webdriver_meta.tti;
        *cleanup = tokio::spawn(async move {
            sleep(tti).await;
            _browsers.write().await.remove(&uuid);
            info!("Removed {:?}", uuid);
        });
    }

    let status_request =
        request.method() == Method::GET && path == format!("/session/driver/{}/status", uuid);

    let mut response = Response::builder();

    debug!("Serving {:?}", uuid);
    let driver_response = proxy_request(
        http,
        &webdriver_meta.protocol,
        browser.address,
        request,
        status_request,
    )
    .await?;
    for (key, value) in driver_response.headers() {
        response = response.header(key.as_str(), value.as_ref());
    }
    response = response.status(driver_response.status().as_u16());

    Ok(response
        .body(Body::from_stream(driver_response.bytes_stream()))
        .map_err(internal_server_error)?)
}

pub async fn proxy_request<S: AsRef<str>>(
    http: Client,
    protocol: S,
    address: SocketAddr,
    request: Request,
    status_request: bool,
) -> Result<reqwest::Response, Response> {
    let method = match request.method() {
        &Method::POST => reqwest::Method::POST,
        &Method::GET => reqwest::Method::GET,
        &Method::DELETE => reqwest::Method::DELETE,
        _ => unreachable!(), // According to specification
    };

    let mut path = if status_request {
        String::from("/status")
    } else {
        String::from(request.uri().path())
    };
    if let Some(query) = request.uri().query() {
        path.push('?');
        path.push_str(query);
    }

    let url = Url::from_str(&format!("{}{}{}", protocol.as_ref(), address, path))
        .map_err(internal_server_error)?;

    let mut header_map = reqwest::header::HeaderMap::new();
    for (key, value) in request.headers() {
        let key =
            reqwest::header::HeaderName::from_bytes(key.as_ref()).map_err(bad_request_error)?;
        let value =
            reqwest::header::HeaderValue::try_from(value.as_bytes()).map_err(bad_request_error)?;
        header_map.insert(key, value);
    }
    header_map.remove(reqwest::header::HOST);

    let bytes = to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(internal_server_error)?;
    let request = http.request(method, url).headers(header_map).body(bytes);
    Ok(request.send().await.map_err(gateway_error)?)
}

pub fn gateway_error<E>(e: E) -> Response
where
    E: std::error::Error,
{
    error!("Gateway Error: {e}");
    (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
}

pub fn internal_server_error<E>(e: E) -> Response
where
    E: std::error::Error,
{
    error!("Internal Server Error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

pub fn bad_request_error<E>(e: E) -> Response
where
    E: std::error::Error,
{
    error!("Bad Request: {e}");
    (StatusCode::BAD_REQUEST, e.to_string()).into_response()
}

pub async fn graceful_shutdown() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
