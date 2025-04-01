use deadpool::managed;
use fantoccini::error::{CmdError, ErrorStatus, NewSessionError};
use fantoccini::wd::Capabilities;
use fantoccini::{Client, ClientBuilder};
use serde::Deserialize;
use std::fmt::{Display, Formatter};
use std::sync::OnceLock;

pub type Pool = managed::Pool<Manager>;

#[derive(Debug)]
pub enum Error {
    CmdError(CmdError),
    ErrorStatus(ErrorStatus),
    NewSessionError(NewSessionError),
    Other(std::io::Error),
    ProxyError(reqwest::Error),
    Stateless,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::CmdError(error) => Display::fmt(error, f),
            Error::ErrorStatus(error) => Display::fmt(error, f),
            Error::NewSessionError(error) => Display::fmt(error, f),
            Error::Other(error) => Display::fmt(error, f),
            Error::Stateless => f.write_str("Client must create a session"),
            Error::ProxyError(error) => Display::fmt(error, f),
        }
    }
}

impl std::error::Error for Error {}

impl From<CmdError> for Error {
    fn from(value: CmdError) -> Self {
        Self::CmdError(value)
    }
}

impl From<ErrorStatus> for Error {
    fn from(value: ErrorStatus) -> Self {
        Self::ErrorStatus(value)
    }
}

impl From<NewSessionError> for Error {
    fn from(value: NewSessionError) -> Self {
        Self::NewSessionError(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Other(value)
    }
}

impl From<reqwest::Error> for Error {
    fn from(value: reqwest::Error) -> Self {
        Self::ProxyError(value)
    }
}

pub struct Config {
    pub webdriver: String,
    pub capabilities: Option<Capabilities>,
    pub disable_ring_provider_init: bool,
}

impl Config {
    pub fn new<S: Into<String>>(webdriver: S, capabilities: Option<Capabilities>) -> Self {
        Self {
            webdriver: webdriver.into(),
            capabilities,
            disable_ring_provider_init: false,
        }
    }

    pub fn disable_ring_provider_init(&mut self) {
        self.disable_ring_provider_init = true;
    }
}

pub struct Manager {
    pub config: Config,
    pub http: reqwest::Client,
}

impl Manager {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }
}

static CRYPTO_PROVIDER_LOCK: OnceLock<()> = OnceLock::new();

fn setup_default_crypto_provider() {
    CRYPTO_PROVIDER_LOCK.get_or_init(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("rustls might already be initialised (You might want to call `disable_ring_provider_init` on your SessionDriver configuration)")
    });
}

impl managed::Manager for Manager {
    type Type = Client;
    type Error = Error;

    async fn create(&self) -> Result<Client, Error> {
        setup_default_crypto_provider();
        let mut builder = ClientBuilder::rustls()?;

        if let Some(capabilities) = &self.config.capabilities {
            builder.capabilities(capabilities.clone());
        }

        let client = builder.connect(&self.config.webdriver).await?;

        Ok(client)
    }

    async fn recycle(
        &self,
        client: &mut Client,
        _: &managed::Metrics,
    ) -> managed::RecycleResult<Error> {
        let session = match client.session_id().await.map_err(Error::from)? {
            Some(session) => session,
            None => return Err(Error::Stateless.into()),
        };

        #[derive(Deserialize)]
        struct Value {
            value: Status,
        }

        #[derive(Deserialize)]
        struct Status {
            ready: bool,
            message: String,
        }

        let request = self
            .http
            .get(format!("/session/driver/{}/status", session))
            .send()
            .await
            .map_err(Error::from)?;
        let status: Value = request.json().await.map_err(Error::from)?;
        if !status.value.ready {
            return Err(Error::ErrorStatus(ErrorStatus::UnknownError).into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fantoccini::Locator;

    #[tokio::test]
    async fn establish_sessions() {
        let mut capabilities = Capabilities::new();
        capabilities.insert(
            "moz:firefoxOptions".to_string(),
            serde_json::json!({
                "args": ["--headless"]
            }),
        );

        let config = Config::new("http://127.0.0.1:4444", Some(capabilities));
        let pool = Pool::builder(Manager::new(config))
            .max_size(5)
            .build()
            .unwrap();

        let mut handles = Vec::new();
        for _ in 0..5 {
            handles.push(pool.get().await.unwrap());
        }
    }

    #[tokio::test]
    async fn session_integrity() {
        let mut capabilities = Capabilities::new();
        capabilities.insert(
            "moz:firefoxOptions".to_string(),
            serde_json::json!({
                "args": ["--headless"]
            }),
        );

        let config = Config::new("http://127.0.0.1:4444", Some(capabilities));
        let pool = Pool::builder(Manager::new(config))
            .max_size(2)
            .build()
            .unwrap();

        {
            let session1 = pool.get().await.unwrap();
            let session2 = pool.get().await.unwrap();

            session1
                .goto("https://de.wikipedia.org/wiki/Clausthal-Zellerfeld")
                .await
                .unwrap();
            session2
                .goto("https://de.wikipedia.org/wiki/Berlin")
                .await
                .unwrap();

            let title1 = session1
                .find(Locator::XPath(r#"//*[@id="firstHeading"]/span"#))
                .await
                .unwrap();
            let title2 = session2
                .find(Locator::XPath(r#"//*[@id="firstHeading"]/span"#))
                .await
                .unwrap();

            let text1 = title1.text().await.unwrap();
            let text2 = title2.text().await.unwrap();

            assert_eq!(text1, "Clausthal-Zellerfeld");
            assert_eq!(text2, "Berlin");
        }

        {
            let _session1 = pool.get().await.unwrap();
            let _session2 = pool.get().await.unwrap();
        }
    }
}
