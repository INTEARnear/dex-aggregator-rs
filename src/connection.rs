use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc,
    time::Duration,
};

use serde::Deserialize;
use serde_json::{json, value::RawValue};
use tokio::sync::{mpsc, oneshot, watch};
use web_time::Instant;

use crate::{
    platform::{self, MaybeSend, Received, Socket},
    request::SwapRequest,
    swap::CheckedQuote,
    types::{CryptoHash, OutcomeWaitUntil, Quote, SwapOutcome},
    validate::UnsafeTransactionsError,
};

pub const ROUTER_SUBSCRIBE_URL: &str = "wss://router.intear.tech/route/subscribe";

#[derive(Debug, Clone)]
pub struct RouterConnectionOptions {
    pub url: String,
    /// Waits twice as long after each attempt to connect, or each error of the router, from this
    /// up to `max_retry_delay`
    pub min_retry_delay: Duration,
    pub max_retry_delay: Duration,
    /// Gives up on an attempt to connect after this long, and tries again
    pub connect_timeout: Duration,
    /// Gives up on an outcome once the router hasn't told it for this long on one connection,
    /// asking again until then when it errs. While the router is offline, outcomes wait for it,
    /// and each connection gives it this long again.
    pub outcome_timeout: Duration,
    /// Pings the router once it's been quiet for this long, and connects again when it doesn't
    /// answer in as long, as a router that went offline without closing the connection, or the
    /// network between them, wouldn't tell. Browsers don't let pages ping.
    #[cfg(not(target_arch = "wasm32"))]
    pub ping_interval: Duration,
}

impl Default for RouterConnectionOptions {
    fn default() -> Self {
        Self {
            url: ROUTER_SUBSCRIBE_URL.to_string(),
            min_retry_delay: Duration::from_millis(250),
            max_retry_delay: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            // The router waits up to 80 seconds for a swap's transactions and balances, so it's a
            // few tries
            outcome_timeout: Duration::from_secs(180),
            #[cfg(not(target_arch = "wasm32"))]
            ping_interval: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Open,
    /// Waits to connect again, after the connection was lost or an attempt to connect failed
    Reconnecting {
        error: String,
    },
    Closed,
}

/// The quotes of a route request
#[derive(Debug)]
pub struct QuoteUpdate {
    /// Safe to sign, best first
    pub quotes: Vec<CheckedQuote>,
    pub rejected: Vec<RejectedQuote>,
}

#[derive(Debug)]
pub struct RejectedQuote {
    /// As the router sent it
    pub json: Box<RawValue>,
    pub error: UnsafeTransactionsError,
}

/// Of a route request, which is sent again after it
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RouterError(pub String);

#[derive(Debug, thiserror::Error)]
pub enum OutcomeError {
    #[error(
        "The router didn't tell the outcome in {:?} on a connection{}",
        .timeout,
        .last_error.as_ref().map(|error| format!(": {error}")).unwrap_or_default()
    )]
    TimedOut {
        timeout: Duration,
        /// Of the router, after which the outcome was asked for again
        last_error: Option<String>,
    },
    #[error("The router's outcome isn't shaped like its outcomes: {0}")]
    Malformed(serde_json::Error),
    #[error("The connection to the router is closed")]
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("The connection to the router is closed")]
pub struct ConnectionClosed;

/// The quotes of the route request, again whenever the pools change
#[derive(Debug)]
pub struct QuoteSubscription {
    updates: mpsc::UnboundedReceiver<Result<QuoteUpdate, RouterError>>,
}

impl QuoteSubscription {
    /// None once another subscription replaced it, or the connection is closed. Dropping it stops
    /// the route request.
    pub async fn next(&mut self) -> Option<Result<QuoteUpdate, RouterError>> {
        self.updates.recv().await
    }
}

/// A connection to the router's `/route/subscribe` that comes back by itself: once it's
/// reconnected, the route request goes on, and the outcomes asked for are asked again, as the
/// router stores quotes by their ID
#[derive(Debug, Clone)]
pub struct RouterConnection {
    commands: mpsc::UnboundedSender<Command>,
    status: watch::Receiver<ConnectionStatus>,
}

impl RouterConnection {
    /// The connection, and the future that keeps it, to spawn: `tokio::spawn(driver)`, or
    /// `wasm_bindgen_futures::spawn_local(driver)` on the web. It ends once the connection is
    /// closed, or every clone of the connection is dropped.
    pub fn new(
        options: RouterConnectionOptions,
    ) -> (Self, impl Future<Output = ()> + MaybeSend + 'static) {
        let (commands_sender, commands) = mpsc::unbounded_channel();
        let (status_sender, status) = watch::channel(ConnectionStatus::Connecting);
        let driver = Driver {
            options,
            commands,
            status: status_sender,
            last_id: 0,
            failed_connects: 0,
            subscription: None,
            outcomes: HashMap::new(),
        };
        let connection = Self {
            commands: commands_sender,
            status,
        };
        (connection, driver.run())
    }

    pub fn status(&self) -> watch::Receiver<ConnectionStatus> {
        self.status.clone()
    }

    /// Routes the request with its fees in the next steps of its outcome, again whenever the pools
    /// change, replacing the previous subscription. Quotes whose transactions aren't safe to sign
    /// are rejected.
    pub fn subscribe(&self, request: SwapRequest) -> Result<QuoteSubscription, ConnectionClosed> {
        let (updates_sender, updates) = mpsc::unbounded_channel();
        self.commands
            .send(Command::Subscribe {
                request: Arc::new(request),
                updates: updates_sender,
            })
            .map_err(|_closed| ConnectionClosed)?;
        Ok(QuoteSubscription { updates })
    }

    /// Of a quote's executed transactions, or only its swap with [`OutcomeWaitUntil::Swap`]. Asks
    /// again after the router's errors, until `outcome_timeout`.
    pub async fn outcome(
        &self,
        quote_id: i64,
        tx_hashes: &[CryptoHash],
        wait_until: OutcomeWaitUntil,
    ) -> Result<SwapOutcome, OutcomeError> {
        let (reply, outcome) = oneshot::channel();
        self.commands
            .send(Command::Outcome {
                quote_id,
                tx_hashes: tx_hashes.to_vec(),
                wait_until,
                reply,
            })
            .map_err(|_closed| OutcomeError::Closed)?;
        outcome.await.map_err(|_closed| OutcomeError::Closed)?
    }

    /// Fails the outcomes it waits for, and ends the subscription
    pub fn close(&self) {
        // Already closed if nothing receives it
        self.commands.send(Command::Close).ok();
    }
}

type Updates = mpsc::UnboundedSender<Result<QuoteUpdate, RouterError>>;

enum Command {
    Subscribe {
        request: Arc<SwapRequest>,
        updates: Updates,
    },
    Outcome {
        quote_id: i64,
        tx_hashes: Vec<CryptoHash>,
        wait_until: OutcomeWaitUntil,
        reply: oneshot::Sender<Result<SwapOutcome, OutcomeError>>,
    },
    Close,
}

/// Why serving a connection stopped
enum Stop {
    Lost(String),
    Closed,
}

/// A request that's sent again after the router's errors
struct Retried {
    message: String,
    failed_attempts: u32,
    /// Unanswered when it isn't waiting to be sent again
    again_at: Option<Instant>,
}

impl Retried {
    fn new(message: String) -> Self {
        Self {
            message,
            failed_attempts: 0,
            again_at: None,
        }
    }

    /// Twice as late as after the previous error
    fn send_again_later(&mut self, options: &RouterConnectionOptions) {
        self.again_at = Some(Instant::now() + retry_delay(options, self.failed_attempts));
        self.failed_attempts += 1;
    }
}

struct Subscription {
    id: u64,
    request: Arc<SwapRequest>,
    updates: Updates,
    retried: Retried,
}

struct PendingOutcome {
    retried: Retried,
    /// Of the connection, without one
    deadline: Option<Instant>,
    last_error: Option<String>,
    reply: oneshot::Sender<Result<SwapOutcome, OutcomeError>>,
}

/// What the router sends
#[derive(Deserialize)]
struct RouterMessage {
    id: u64,
    routes: Option<Vec<Box<RawValue>>>,
    outcome: Option<Box<RawValue>>,
    error: Option<String>,
}

struct Driver {
    options: RouterConnectionOptions,
    commands: mpsc::UnboundedReceiver<Command>,
    status: watch::Sender<ConnectionStatus>,
    last_id: u64,
    failed_connects: u32,
    subscription: Option<Subscription>,
    outcomes: HashMap<u64, PendingOutcome>,
}

impl Driver {
    async fn run(mut self) {
        let mut delay = Duration::ZERO;
        while let Some(mut socket) = self.connect(delay).await {
            match self.serve(&mut socket).await {
                Stop::Closed => {
                    socket.close().await;
                    break;
                }
                Stop::Lost(error) => {
                    for pending in self.outcomes.values_mut() {
                        pending.deadline = None;
                    }
                    self.status
                        .send_replace(ConnectionStatus::Reconnecting { error });
                    delay = retry_delay(&self.options, self.failed_connects);
                    self.failed_connects += 1;
                }
            }
        }
        self.status.send_replace(ConnectionStatus::Closed);
        for pending in self.outcomes.into_values() {
            // The caller may have stopped waiting
            pending.reply.send(Err(OutcomeError::Closed)).ok();
        }
    }

    /// Once it waited the delay, and again after each attempt that failed, while it goes on with
    /// the commands. None once the connection is closed.
    async fn connect(&mut self, mut delay: Duration) -> Option<Socket> {
        loop {
            let options = self.options.clone();
            let connect_timeout = self.options.connect_timeout;
            let mut attempt = std::pin::pin!(async move {
                platform::sleep_until(Some(Instant::now() + delay)).await;
                tokio::select! {
                    connected = platform::connect(&options) => connected,
                    () = platform::sleep_until(Some(Instant::now() + connect_timeout)) => {
                        Err(format!("Didn't connect in {}s", connect_timeout.as_secs()))
                    }
                }
            });
            let connected = loop {
                let next_timer = self.next_timer();
                let handled = tokio::select! {
                    connected = &mut attempt => break connected,
                    command = self.commands.recv() => self.command(command, None).await,
                    () = platform::sleep_until(next_timer) => self.timers(None).await,
                    () = subscription_dropped(self.subscription.as_ref()) => {
                        self.unsubscribe(None).await
                    }
                };
                // Without a connection to lose, only closing it stops
                if handled.is_err() {
                    return None;
                }
            };
            match connected {
                Ok(socket) => {
                    self.failed_connects = 0;
                    self.status.send_replace(ConnectionStatus::Open);
                    return Some(socket);
                }
                Err(error) => {
                    self.status
                        .send_replace(ConnectionStatus::Reconnecting { error });
                    delay = retry_delay(&self.options, self.failed_connects);
                    self.failed_connects += 1;
                }
            }
        }
    }

    async fn serve(&mut self, socket: &mut Socket) -> Stop {
        let deadline = Instant::now() + self.options.outcome_timeout;
        for pending in self.outcomes.values_mut() {
            pending.deadline = Some(deadline);
        }
        // The others are sent again once they waited after the router's error
        let unanswered = self
            .requests()
            .filter(|retried| retried.again_at.is_none())
            .map(|retried| retried.message.clone())
            .collect::<Vec<_>>();
        for message in unanswered {
            if let Err(stop) = send(Some(&mut *socket), &message).await {
                return stop;
            }
        }
        loop {
            let next_timer = self
                .next_timer()
                .into_iter()
                .chain(socket.keep_alive_at())
                .min();
            let handled = tokio::select! {
                received = socket.receive() => self.received(received),
                command = self.commands.recv() => self.command(command, Some(&mut *socket)).await,
                () = platform::sleep_until(next_timer) => {
                    match self.timers(Some(&mut *socket)).await {
                        Ok(()) => socket.keep_alive().await.map_err(Stop::Lost),
                        Err(stop) => Err(stop),
                    }
                }
                () = subscription_dropped(self.subscription.as_ref()) => {
                    self.unsubscribe(Some(&mut *socket)).await
                }
            };
            if let Err(stop) = handled {
                return stop;
            }
        }
    }

    fn new_id(&mut self) -> u64 {
        self.last_id += 1;
        self.last_id
    }

    fn requests(&self) -> impl Iterator<Item = &Retried> {
        let subscription = self
            .subscription
            .iter()
            .map(|subscription| &subscription.retried);
        subscription.chain(self.outcomes.values().map(|pending| &pending.retried))
    }

    fn next_timer(&self) -> Option<Instant> {
        let retries = self.requests().filter_map(|retried| retried.again_at);
        let deadlines = self
            .outcomes
            .values()
            .filter_map(|pending| pending.deadline);
        retries.chain(deadlines).min()
    }

    async fn command(
        &mut self,
        command: Option<Command>,
        socket: Option<&mut Socket>,
    ) -> Result<(), Stop> {
        let message = match command {
            // Every clone of the connection was dropped
            None | Some(Command::Close) => return Err(Stop::Closed),
            Some(Command::Subscribe { request, updates }) => {
                let id = self.new_id();
                let message = json!({
                    "id": id,
                    "query": request.route_query(),
                    "fees_in_next_steps": true,
                })
                .to_string();
                // The updates of the one it replaces end, and the router stops routing it
                self.subscription = Some(Subscription {
                    id,
                    request,
                    updates,
                    retried: Retried::new(message.clone()),
                });
                message
            }
            Some(Command::Outcome {
                quote_id,
                tx_hashes,
                wait_until,
                reply,
            }) => {
                let id = self.new_id();
                let message = json!({
                    "id": id,
                    "outcome": {
                        "quote_id": quote_id,
                        "tx_hashes": tx_hashes,
                        "wait_until": wait_until,
                    },
                })
                .to_string();
                let deadline = Instant::now() + self.options.outcome_timeout;
                self.outcomes.insert(
                    id,
                    PendingOutcome {
                        retried: Retried::new(message.clone()),
                        deadline: socket.is_some().then_some(deadline),
                        last_error: None,
                        reply,
                    },
                );
                message
            }
        };
        send(socket, &message).await
    }

    /// Of a subscription whose updates nobody receives anymore
    async fn unsubscribe(&mut self, socket: Option<&mut Socket>) -> Result<(), Stop> {
        self.subscription = None;
        let id = self.new_id();
        // A message without a query stops routing
        send(socket, &json!({ "id": id }).to_string()).await
    }

    async fn timers(&mut self, mut socket: Option<&mut Socket>) -> Result<(), Stop> {
        let now = Instant::now();
        for (_, pending) in self
            .outcomes
            .extract_if(|_, pending| pending.deadline.is_some_and(|deadline| deadline <= now))
        {
            let timed_out = OutcomeError::TimedOut {
                timeout: self.options.outcome_timeout,
                last_error: pending.last_error,
            };
            pending.reply.send(Err(timed_out)).ok();
        }
        let subscription = self
            .subscription
            .iter_mut()
            .map(|subscription| &mut subscription.retried);
        let due = subscription
            .chain(
                self.outcomes
                    .values_mut()
                    .map(|pending| &mut pending.retried),
            )
            .filter(|retried| retried.again_at.is_some_and(|again_at| again_at <= now))
            .map(|retried| {
                retried.again_at = None;
                retried.message.clone()
            })
            .collect::<Vec<_>>();
        for message in due {
            send(socket.as_deref_mut(), &message).await?;
        }
        Ok(())
    }

    fn received(&mut self, received: Received) -> Result<(), Stop> {
        match received {
            Received::Text(text) => {
                self.receive(&text);
                Ok(())
            }
            Received::Other => Ok(()),
            Received::Closed(error) => Err(Stop::Lost(error)),
        }
    }

    fn receive(&mut self, text: &str) {
        let message: RouterMessage = match serde_json::from_str(text) {
            Ok(message) => message,
            Err(error) => {
                if let Some(subscription) = &self.subscription {
                    let error =
                        format!("The router sent something else than a message ({error}): {text}");
                    subscription.updates.send(Err(RouterError(error))).ok();
                }
                return;
            }
        };
        if let Entry::Occupied(mut entry) = self.outcomes.entry(message.id) {
            match message.outcome {
                Some(outcome) => {
                    let outcome =
                        serde_json::from_str(outcome.get()).map_err(OutcomeError::Malformed);
                    // The caller may have stopped waiting
                    entry.remove().reply.send(outcome).ok();
                }
                None => {
                    let pending = entry.get_mut();
                    pending.last_error =
                        Some(message.error.unwrap_or_else(|| {
                            "The router answered without an outcome".to_string()
                        }));
                    pending.retried.send_again_later(&self.options);
                }
            }
            return;
        }
        // Of an earlier route request otherwise
        let Some(subscription) = self
            .subscription
            .as_mut()
            .filter(|subscription| subscription.id == message.id)
        else {
            return;
        };
        let update = match message.routes {
            Some(routes) => {
                subscription.retried.failed_attempts = 0;
                Ok(checked_quotes(&subscription.request, routes))
            }
            None => {
                subscription.retried.send_again_later(&self.options);
                Err(RouterError(message.error.unwrap_or_else(|| {
                    "The router answered without routes".to_string()
                })))
            }
        };
        // Dropped if nothing receives it, which `subscription_dropped` handles
        subscription.updates.send(update).ok();
    }
}

fn checked_quotes(request: &Arc<SwapRequest>, routes: Vec<Box<RawValue>>) -> QuoteUpdate {
    let mut update = QuoteUpdate {
        quotes: Vec::new(),
        rejected: Vec::new(),
    };
    for json in routes {
        let checked = serde_json::from_str::<Quote>(json.get())
            .map_err(|error| UnsafeTransactionsError {
                problems: vec![format!(
                    "The quote isn't shaped like the router's quotes: {error}"
                )],
            })
            .and_then(|quote| CheckedQuote::new(request.clone(), quote));
        match checked {
            Ok(quote) => update.quotes.push(quote),
            Err(error) => update.rejected.push(RejectedQuote { json, error }),
        }
    }
    update
}

/// Waits twice as long after each attempt that failed
fn retry_delay(options: &RouterConnectionOptions, failed_attempts: u32) -> Duration {
    options
        .min_retry_delay
        .saturating_mul(2u32.saturating_pow(failed_attempts))
        .min(options.max_retry_delay)
}

/// Without a connection, it's sent once connected
async fn send(socket: Option<&mut Socket>, message: &str) -> Result<(), Stop> {
    match socket {
        Some(socket) => socket.send(message).await.map_err(Stop::Lost),
        None => Ok(()),
    }
}

async fn subscription_dropped(subscription: Option<&Subscription>) {
    match subscription {
        Some(subscription) => subscription.updates.closed().await,
        None => std::future::pending().await,
    }
}
