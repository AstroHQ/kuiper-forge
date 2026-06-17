//! Agent runtime — drives the coordinator communication loop for a VM agent.
//!
//! This is a toolkit, not a framework: it owns the mechanical, identical-for-every-
//! agent parts of speaking to the coordinator — connecting (with reconnect/backoff),
//! the bidirectional gRPC stream, `Ping`/`Pong`, and status pushing — and hands the
//! agent two channels via [`Connection`]:
//!
//! - [`Connection::commands`] — a stream of [`RunnerCommand`]s to react to.
//! - [`Connection::events`] — an [`EventSender`] for the acks and runner-lifecycle
//!   events the agent generates.
//!
//! The agent owns its own loop: read a command, do its provider-specific VM work,
//! emit events. Current status flows the other way through a [`watch`] channel —
//! the agent updates it whenever its state changes, and the runtime pushes the
//! latest value to the coordinator (on change and on a timer). `watch` is the right
//! fit because status is latest-wins state; the lossless `events` channel carries
//! the discrete events that must not be coalesced.
//!
//! Any agent (these two, or one a third party writes) gets the same correct
//! connection behaviour for free, while keeping full control of its own logic.

use crate::{AgentCertStore, AgentConfig, AgentConnector, Error, Result};
use kuiper_agent_proto::{
    AgentMessage, AgentPayload, AgentServiceClient, AgentStatus, CommandAck, CoordinatorMessage,
    CoordinatorPayload, CreateRunnerCommand, DestroyRunnerCommand, Ping, Pong, RunnerEvent,
    RunnerEventType,
};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

/// How often to push an unsolicited status update, independent of changes.
const STATUS_INTERVAL: Duration = Duration::from_secs(30);

/// Bounded capacity for the command and event channels.
const CHANNEL_CAPACITY: usize = 32;

/// A command from the coordinator the agent must act on.
///
/// `Ping` is handled by the runtime and never surfaces here.
#[derive(Debug)]
pub enum RunnerCommand {
    /// Create a runner VM and run its lifecycle.
    Create(CreateRunnerCommand),
    /// Destroy a runner VM.
    Destroy(DestroyRunnerCommand),
}

/// Handle for sending the events an agent generates back to the coordinator.
///
/// Cloneable, so spawned per-runner tasks can each hold one. Sends are lossless
/// and ordered; they buffer briefly across a reconnect and flush on reconnect.
#[derive(Clone)]
pub struct EventSender {
    tx: mpsc::Sender<AgentMessage>,
}

impl EventSender {
    /// Acknowledge (accept or reject) a command from the coordinator.
    pub async fn command_ack(
        &self,
        command_id: String,
        accepted: bool,
        error: String,
    ) -> Result<()> {
        self.send(AgentPayload::Ack(CommandAck {
            command_id,
            accepted,
            error,
        }))
        .await
    }

    /// Report a runner lifecycle event (started/completed/failed/destroyed).
    pub async fn runner_event(
        &self,
        runner_name: String,
        vm_id: String,
        event_type: RunnerEventType,
        error: String,
    ) -> Result<()> {
        self.send(AgentPayload::RunnerEvent(RunnerEvent {
            runner_name,
            vm_id,
            event_type: event_type as i32,
            error,
        }))
        .await
    }

    async fn send(&self, payload: AgentPayload) -> Result<()> {
        self.tx
            .send(AgentMessage {
                payload: Some(payload),
            })
            .await
            .map_err(|_| Error::ChannelSend)
    }
}

/// A live connection to the coordinator, driven by a background task.
///
/// The agent reacts to [`commands`](Self::commands) and emits via
/// [`events`](Self::events). Dropping the `Connection` stops the driver.
pub struct Connection {
    /// Commands from the coordinator to act on.
    pub commands: mpsc::Receiver<RunnerCommand>,
    /// Sender for agent-generated events (acks, runner events).
    pub events: EventSender,
}

/// Start the background driver and return the [`Connection`] the agent works with.
///
/// The driver connects to the coordinator (reconnecting with exponential backoff,
/// capped at `max_delay`), runs the status stream, answers pings, forwards
/// Create/Destroy commands, and sends agent events. `status` is the agent's live
/// status; the driver pushes its latest value on change and every
/// [`STATUS_INTERVAL`].
pub fn connect(
    config: AgentConfig,
    cert_store: AgentCertStore,
    status: watch::Receiver<AgentStatus>,
    initial_delay: Duration,
    max_delay: Duration,
) -> Connection {
    let (command_tx, command_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(CHANNEL_CAPACITY);

    tokio::spawn(drive(
        config,
        cert_store,
        status,
        command_tx,
        event_rx,
        initial_delay,
        max_delay,
    ));

    Connection {
        commands: command_rx,
        events: EventSender { tx: event_tx },
    }
}

/// Background driver: connect, run a session, reconnect with backoff, repeat.
async fn drive(
    config: AgentConfig,
    cert_store: AgentCertStore,
    status: watch::Receiver<AgentStatus>,
    command_tx: mpsc::Sender<RunnerCommand>,
    mut event_rx: mpsc::Receiver<AgentMessage>,
    initial_delay: Duration,
    max_delay: Duration,
) {
    let mut delay = initial_delay;
    loop {
        let mut connector = AgentConnector::new(config.clone(), cert_store.clone());
        match connector.connect().await {
            Ok(client) => {
                info!("Connected to coordinator");
                if let Some(agent_id) = connector.agent_id() {
                    info!("Agent ID: {}", agent_id);
                }
                delay = initial_delay; // reset backoff after a successful connect
                if let Err(e) = run_session(client, &status, &command_tx, &mut event_rx).await {
                    warn!("Stream ended: {}", e);
                } else {
                    info!("Stream closed by coordinator");
                }
            }
            Err(e) => error!("Connection failed: {}", e),
        }

        // The agent dropped its Connection (e.g. shutting down) — stop driving.
        if command_tx.is_closed() {
            debug!("Connection dropped by agent; stopping runtime driver");
            return;
        }

        info!("Reconnecting in {:?}...", delay);
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

/// Run a single stream session until it closes or errors.
async fn run_session(
    mut client: AgentServiceClient<Channel>,
    status: &watch::Receiver<AgentStatus>,
    command_tx: &mpsc::Sender<RunnerCommand>,
    event_rx: &mut mpsc::Receiver<AgentMessage>,
) -> Result<()> {
    let (out_tx, out_rx) = mpsc::channel::<AgentMessage>(CHANNEL_CAPACITY);

    // The server expects the first message to identify the agent. Bind the clone
    // first so the (non-Send) watch guard drops before the await.
    let initial = status.borrow().clone();
    out_tx
        .send(status_msg(initial))
        .await
        .map_err(|_| Error::ChannelSend)?;

    let response = client.agent_stream(ReceiverStream::new(out_rx)).await?;
    let mut inbound = response.into_inner();
    info!("Sent initial status to coordinator");

    // Status pusher: forwards the latest status on every change and on a timer.
    // Critical for recovery — the coordinator relies on these to learn when VMs
    // complete and to keep its capacity view fresh.
    let pusher = tokio::spawn(push_status(out_tx.clone(), status.clone()));

    let result = async {
        loop {
            tokio::select! {
                inbound_msg = inbound.message() => {
                    match inbound_msg? {
                        Some(msg) => handle_inbound(msg, &out_tx, command_tx, status).await?,
                        None => return Ok(()), // stream closed by coordinator
                    }
                }
                event = event_rx.recv() => {
                    match event {
                        Some(msg) => out_tx.send(msg).await.map_err(|_| Error::ChannelSend)?,
                        None => return Ok(()), // agent dropped its EventSender(s)
                    }
                }
            }
        }
    }
    .await;

    pusher.abort();
    result
}

/// Push the latest status to `out_tx` on every change and every [`STATUS_INTERVAL`].
async fn push_status(out_tx: mpsc::Sender<AgentMessage>, mut status: watch::Receiver<AgentStatus>) {
    let mut ticker = tokio::time::interval(STATUS_INTERVAL);
    ticker.tick().await; // skip the first tick — initial status was already sent
    status.borrow_and_update(); // current value already sent; wait for the next change

    loop {
        let latest = tokio::select! {
            changed = status.changed() => {
                if changed.is_err() {
                    break; // status sender dropped
                }
                status.borrow_and_update().clone()
            }
            _ = ticker.tick() => status.borrow().clone(),
        };
        if out_tx.send(status_msg(latest)).await.is_err() {
            break; // stream is ending
        }
    }
}

/// Handle one inbound coordinator message.
async fn handle_inbound(
    msg: CoordinatorMessage,
    out_tx: &mpsc::Sender<AgentMessage>,
    command_tx: &mpsc::Sender<RunnerCommand>,
    status: &watch::Receiver<AgentStatus>,
) -> Result<()> {
    let Some(payload) = msg.payload else {
        debug!("Received empty message from coordinator");
        return Ok(());
    };

    match payload {
        CoordinatorPayload::Ping(Ping {}) => {
            debug!("Received ping from coordinator");
            out_tx
                .send(AgentMessage {
                    payload: Some(AgentPayload::Pong(Pong {})),
                })
                .await
                .map_err(|_| Error::ChannelSend)?;
            // Bind the clone first so the (non-Send) watch guard drops before the await.
            let current = status.borrow().clone();
            out_tx
                .send(status_msg(current))
                .await
                .map_err(|_| Error::ChannelSend)?;
        }
        CoordinatorPayload::CreateRunner(cmd) => {
            info!(
                "Received CreateRunner command: {} ({})",
                cmd.command_id, cmd.vm_name
            );
            command_tx
                .send(RunnerCommand::Create(cmd))
                .await
                .map_err(|_| Error::ChannelSend)?;
        }
        CoordinatorPayload::DestroyRunner(cmd) => {
            info!(
                "Received DestroyRunner command: {} ({})",
                cmd.command_id, cmd.vm_id
            );
            command_tx
                .send(RunnerCommand::Destroy(cmd))
                .await
                .map_err(|_| Error::ChannelSend)?;
        }
    }

    Ok(())
}

fn status_msg(status: AgentStatus) -> AgentMessage {
    AgentMessage {
        payload: Some(AgentPayload::Status(status)),
    }
}
