use web_time::Instant;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{Socket, connect};
#[cfg(target_arch = "wasm32")]
pub(crate) use web::{Socket, connect};

/// `Send` on native targets. On the web, where futures aren't, implemented by every type.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// `Sync` on native targets. On the web implemented by every type, like [`MaybeSend`].
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSync: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync + ?Sized> MaybeSync for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSync for T {}

/// Never wakes without an instant
pub(crate) async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => futures_timer::Delay::new(at.saturating_duration_since(Instant::now())).await,
        None => std::future::pending().await,
    }
}

/// What the router sent
pub(crate) enum Received {
    Text(String),
    Other,
    Closed(String),
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::{
        sync::{Arc, LazyLock},
        time::Duration,
    };

    use futures_util::{SinkExt, StreamExt};
    use rustls::{ClientConfig, RootCertStore};
    use tokio::net::TcpStream;
    use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream, tungstenite::Message};
    use web_time::Instant;

    use super::Received;
    use crate::RouterConnectionOptions;

    /// Its own config, with the provider named, as near-api and near-kit compile in two
    /// providers, of which rustls picks none by itself
    static TLS_CONFIG: LazyLock<Result<Arc<ClientConfig>, String>> = LazyLock::new(|| {
        let certificates = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(certificates.certs);
        if roots.is_empty() {
            return Err(format!(
                "Found no root certificates of the system: {:?}",
                certificates.errors
            ));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| error.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Arc::new(config))
    });

    /// Pings the router once it's been quiet for `ping_interval`, and takes the connection for
    /// lost when the router doesn't answer in as long: a router that went offline without
    /// closing it, or the network between them, wouldn't tell
    pub(crate) struct Socket {
        stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        ping_interval: Duration,
        last_heard: Instant,
        pinged_at: Option<Instant>,
    }

    pub(crate) async fn connect(options: &RouterConnectionOptions) -> Result<Socket, String> {
        let connector = Connector::Rustls(TLS_CONFIG.clone()?);
        let (stream, _response) = tokio_tungstenite::connect_async_tls_with_config(
            &options.url,
            None,
            false,
            Some(connector),
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(Socket {
            stream,
            ping_interval: options.ping_interval,
            last_heard: Instant::now(),
            pinged_at: None,
        })
    }

    impl Socket {
        pub(crate) async fn send(&mut self, text: &str) -> Result<(), String> {
            self.stream
                .send(Message::text(text))
                .await
                .map_err(|error| error.to_string())
        }

        pub(crate) async fn receive(&mut self) -> Received {
            let message = self.stream.next().await;
            // Whatever it sends, it's there
            self.last_heard = Instant::now();
            self.pinged_at = None;
            match message {
                Some(Ok(Message::Text(text))) => Received::Text(text.as_str().to_string()),
                Some(Ok(Message::Close(_))) | None => {
                    Received::Closed("The router closed the connection".to_string())
                }
                Some(Err(error)) => Received::Closed(error.to_string()),
                Some(Ok(_)) => Received::Other,
            }
        }

        /// When it pings the router, or gives up on its answer
        pub(crate) fn keep_alive_at(&self) -> Option<Instant> {
            Some(self.pinged_at.unwrap_or(self.last_heard) + self.ping_interval)
        }

        pub(crate) async fn keep_alive(&mut self) -> Result<(), String> {
            let now = Instant::now();
            if self
                .keep_alive_at()
                .is_some_and(|keep_alive_at| keep_alive_at > now)
            {
                return Ok(());
            }
            if self.pinged_at.is_some() {
                return Err(format!(
                    "The router didn't answer a ping in {}ms",
                    self.ping_interval.as_millis()
                ));
            }
            self.stream
                .send(Message::Ping(Default::default()))
                .await
                .map_err(|error| error.to_string())?;
            self.pinged_at = Some(now);
            Ok(())
        }

        pub(crate) async fn close(&mut self) {
            // Gone already if it fails
            self.stream.close(None).await.ok();
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite_wasm::{Message, WebSocketStream};
    use web_time::Instant;

    use super::Received;
    use crate::RouterConnectionOptions;

    /// The browser's, which doesn't let pages ping, so a connection that died without closing is
    /// only lost once the browser finds out
    pub(crate) struct Socket(WebSocketStream);

    pub(crate) async fn connect(options: &RouterConnectionOptions) -> Result<Socket, String> {
        tokio_tungstenite_wasm::connect(&options.url)
            .await
            .map(Socket)
            .map_err(|error| error.to_string())
    }

    impl Socket {
        pub(crate) async fn send(&mut self, text: &str) -> Result<(), String> {
            self.0
                .send(Message::text(text))
                .await
                .map_err(|error| error.to_string())
        }

        pub(crate) async fn receive(&mut self) -> Received {
            match self.0.next().await {
                Some(Ok(Message::Text(text))) => Received::Text(text.as_str().to_string()),
                Some(Ok(Message::Close(_))) | None => {
                    Received::Closed("The router closed the connection".to_string())
                }
                Some(Err(error)) => Received::Closed(error.to_string()),
                Some(Ok(Message::Binary(_))) => Received::Other,
            }
        }

        pub(crate) fn keep_alive_at(&self) -> Option<Instant> {
            None
        }

        pub(crate) async fn keep_alive(&mut self) -> Result<(), String> {
            Ok(())
        }

        pub(crate) async fn close(&mut self) {
            // Gone already if it fails
            self.0.close().await.ok();
        }
    }
}
