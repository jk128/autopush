//! Management of connected clients to a WebPush server
//!
//! This module is a pretty heavy work in progress. The intention is that
//! this'll house all the various state machine transitions and state management
//! of connected clients. Note that it's expected there'll be a lot of connected
//! clients, so this may appears relatively heavily optimized!

use std::rc::Rc;

use cadence::prelude::*;
use futures::AsyncSink;
use futures::future::Either;
use futures::sync::mpsc;
use futures::sync::oneshot::Receiver;
use futures::{Stream, Sink, Future, Poll, Async};
use tokio_core::reactor::Timeout;
use time;
use uuid::Uuid;
use woothee::parser::{Parser, WootheeResult};

use call;
use errors::*;
use protocol::{ClientAck, ClientMessage, ServerMessage, ServerNotification, Notification};
use server::Server;

pub struct RegisteredClient {
    pub uaid: Uuid,
    pub tx: mpsc::UnboundedSender<ServerNotification>,
}

// Websocket session statistics
#[derive(Clone)]
struct SessionStatistics {
    // User data
    uaid: String,
    uaid_reset: bool,
    existing_uaid: bool,
    connection_type: String,
    host: String,

    // Usage data
    direct_acked: i32,
    direct_storage: i32,
    stored_retrieved: i32,
    stored_acked: i32,
    nacks: i32,
    unregisters: i32,
    registers: i32,
}

// Represents a websocket client connection that may or may not be authenticated
pub struct Client<T> {
    data: ClientData<T>,
    state: ClientState,
}

pub struct ClientData<T> {
    webpush: Option<WebPushClient>,
    srv: Rc<Server>,
    ws: T,
    user_agent: String,
    host: String,
}

// Represent the state for a valid WebPush client that is authenticated
pub struct WebPushClient {
    uaid: Uuid,
    rx: mpsc::UnboundedReceiver<ServerNotification>,
    flags: ClientFlags,
    message_month: String,
    unacked_direct_notifs: Vec<Notification>,
    unacked_stored_notifs: Vec<Notification>,
    // Highest version from stored, retained for use with increment
    // when all the unacked storeds are ack'd
    unacked_stored_highest: Option<i64>,
    connected_at: u64,
    stats: SessionStatistics,
}

impl WebPushClient {
    fn unacked_messages(&self) -> bool {
        self.unacked_stored_notifs.len() > 0 || self.unacked_direct_notifs.len() > 0
    }
}

pub struct ClientFlags {
    include_topic: bool,
    increment_storage: bool,
    check: bool,
    reset_uaid: bool,
    rotate_message_table: bool,
}

impl ClientFlags {
    fn new() -> ClientFlags {
        ClientFlags {
            include_topic: true,
            increment_storage: false,
            check: false,
            reset_uaid: false,
            rotate_message_table: false,
        }
    }

    pub fn none(&self) -> bool {
        // Indicate if none of the flags are true.
        match *self {
            ClientFlags {
                include_topic: false,
                increment_storage: false,
                check: false,
                reset_uaid: false,
                rotate_message_table: false,
            } => true,
            _ => false,
        }
    }
}

pub enum ClientState {
    WaitingForHello(Timeout),
    WaitingForProcessHello(MyFuture<call::HelloResponse>),
    WaitingForRegister(Uuid, MyFuture<call::RegisterResponse>),
    WaitingForUnRegister(Uuid, MyFuture<call::UnRegisterResponse>),
    WaitingForCheckStorage(MyFuture<call::CheckStorageResponse>),
    WaitingForDelete(MyFuture<call::DeleteMessageResponse>),
    WaitingForIncrementStorage(MyFuture<call::IncStorageResponse>),
    WaitingForDropUser(MyFuture<call::DropUserResponse>),
    WaitingForMigrateUser(MyFuture<call::MigrateUserResponse>),
    FinishSend(Option<ServerMessage>, Option<Box<ClientState>>),
    SendMessages(Option<Vec<Notification>>),
    CheckStorage,
    IncrementStorage,
    WaitingForAcks,
    Await,
    Done,
    ShutdownCleanup(Option<Error>),
}

impl<T> Client<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    /// Spins up a new client communicating over the websocket `ws` specified.
    ///
    /// The `ws` specified already has ping/pong parts of the websocket
    /// protocol managed elsewhere, and this struct is only expected to deal
    /// with webpush-specific messages.
    ///
    /// The `srv` argument is the server that this client is attached to and
    /// the various state behind the server. This provides transitive access to
    /// various configuration options of the server as well as the ability to
    /// call back into Python.
    pub fn new(ws: T, srv: &Rc<Server>, mut uarx: Receiver<String>, host: String) -> Client<T> {
        let srv = srv.clone();
        let timeout = Timeout::new(srv.opts.open_handshake_timeout.unwrap(), &srv.handle).unwrap();

        // Pull out the user-agent, which we should have by now
        let uastr = match uarx.poll() {
            Ok(Async::Ready(ua)) => ua,
            Ok(Async::NotReady) => {
                error!("Failed to parse the user-agent");
                String::from("")
            }
            Err(_) => {
                error!("Failed to receive a value");
                String::from("")
            }
        };

        Client {
            state: ClientState::WaitingForHello(timeout),
            data: ClientData {
                webpush: None,
                srv: srv.clone(),
                ws: ws,
                user_agent: uastr,
                host,
            },
        }
    }

    pub fn shutdown(&mut self) {
        self.data.shutdown();
    }

    fn transition(&mut self) -> Poll<ClientState, Error> {
        let host = self.data.host.clone();
        let next_state = match self.state {
            ClientState::FinishSend(None, None) => {
                return Err("Bad state, should not have nothing to do".into())
            }
            ClientState::FinishSend(None, ref mut next_state) => {
                debug!("State: FinishSend w/next_state");
                try_ready!(self.data.ws.poll_complete());
                *next_state.take().unwrap()
            }
            ClientState::FinishSend(ref mut msg, ref mut next_state) => {
                debug!("State: FinishSend w/msg & next_state");
                let item = msg.take().unwrap();
                let ret = self.data.ws.start_send(item).chain_err(|| "unable to send")?;
                match ret {
                    AsyncSink::Ready => {
                        ClientState::FinishSend(None, Some(next_state.take().unwrap()))
                    }
                    AsyncSink::NotReady(returned) => {
                        *msg = Some(returned);
                        return Ok(Async::NotReady);
                    }
                }
            }
            ClientState::SendMessages(ref mut more_messages) => {
                debug!("State: SendMessages");
                if more_messages.is_some() {
                    let mut messages = more_messages.take().unwrap();
                    if let Some(message) = messages.pop() {
                        if let Some(_) = message.topic {
                            self.data.srv.metrics.incr("ua.notification.topic")?;
                        }
                        // XXX: tags
                        self.data.srv.metrics.count(
                            "ua.message_data",
                            message.data.as_ref().map_or(0, |d| {
                                d.len() as i64
                            }),
                        )?;
                        ClientState::FinishSend(
                            Some(ServerMessage::Notification(message)),
                            Some(Box::new(ClientState::SendMessages(if messages.len() > 0 {
                                Some(messages)
                            } else {
                                None
                            }))),
                        )
                    } else {
                        ClientState::SendMessages(if messages.len() > 0 {
                            Some(messages)
                        } else {
                            None
                        })
                    }
                } else {
                    ClientState::WaitingForAcks
                }
            }
            ClientState::CheckStorage => {
                debug!("State: CheckStorage");
                let webpush = self.data.webpush.as_ref().unwrap();
                ClientState::WaitingForCheckStorage(self.data.srv.check_storage(
                    webpush.uaid.simple().to_string(),
                    webpush.message_month.clone(),
                    webpush.flags.include_topic,
                    webpush.unacked_stored_highest,
                ))
            }
            ClientState::IncrementStorage => {
                debug!("State: IncrementStorage");
                let webpush = self.data.webpush.as_ref().unwrap();
                ClientState::WaitingForIncrementStorage(self.data.srv.increment_storage(
                    webpush.uaid.simple().to_string(),
                    webpush.message_month.clone(),
                    webpush.unacked_stored_highest.unwrap(),
                ))
            }
            ClientState::WaitingForHello(ref mut timeout) => {
                debug!("State: WaitingForHello");
                let uaid = match try_ready!(self.data.input_with_timeout(timeout)) {
                    ClientMessage::Hello {
                        uaid,
                        use_webpush: Some(true),
                        ..
                    } => uaid,
                    _ => return Err("Invalid message, must be hello".into()),
                };
                let connected_at = time::precise_time_ns() / 1000;
                ClientState::WaitingForProcessHello(
                    self.data.srv.hello(&connected_at, uaid.as_ref()),
                )
            }
            ClientState::WaitingForProcessHello(ref mut response) => {
                debug!("State: WaitingForProcessHello");
                match try_ready!(response.poll()) {
                    call::HelloResponse {
                        uaid: Some(uaid),
                        message_month,
                        check_storage,
                        reset_uaid,
                        rotate_message_table,
                        connected_at,
                    } => {
                        self.data.process_hello(
                            uaid,
                            message_month,
                            reset_uaid,
                            rotate_message_table,
                            check_storage,
                            connected_at,
                        )
                    }
                    call::HelloResponse { uaid: None, .. } => {
                        return Err("Already connected elsewhere".into())
                    }
                }
            }
            ClientState::WaitingForCheckStorage(ref mut response) => {
                debug!("State: WaitingForCheckStorage");
                let (include_topic, mut messages, timestamp) = match try_ready!(response.poll()) {
                    call::CheckStorageResponse {
                        include_topic,
                        messages,
                        timestamp,
                    } => (include_topic, messages, timestamp),
                };
                debug!("Got checkstorage response");
                let webpush = self.data.webpush.as_mut().unwrap();
                webpush.flags.include_topic = include_topic;
                webpush.unacked_stored_highest = timestamp;
                if messages.len() > 0 {
                    webpush.flags.increment_storage = !include_topic;
                    webpush.unacked_stored_notifs.extend(
                        messages.iter().cloned(),
                    );
                    let message = ServerMessage::Notification(messages.pop().unwrap());
                    ClientState::FinishSend(
                        Some(message),
                        Some(Box::new(ClientState::SendMessages(Some(messages)))),
                    )
                } else {
                    webpush.flags.check = false;
                    ClientState::Await
                }
            }
            ClientState::WaitingForIncrementStorage(ref mut response) => {
                debug!("State: WaitingForIncrementStorage");
                try_ready!(response.poll());
                self.data.webpush.as_mut().unwrap().flags.increment_storage = false;
                ClientState::WaitingForAcks
            }
            ClientState::WaitingForMigrateUser(ref mut response) => {
                debug!("State: WaitingForMigrateUser");
                let message_month = match try_ready!(response.poll()) {
                    call::MigrateUserResponse { message_month } => message_month,
                };
                let webpush = self.data.webpush.as_mut().unwrap();
                webpush.message_month = message_month;
                webpush.flags.rotate_message_table = false;
                ClientState::Await
            }
            ClientState::WaitingForRegister(channel_id, ref mut response) => {
                debug!("State: WaitingForRegister");
                let msg = match try_ready!(response.poll()) {
                    call::RegisterResponse::Success { endpoint } => {
                        self.data.webpush.as_mut().unwrap().stats.registers += 1;
                        ServerMessage::Register {
                            channel_id: channel_id,
                            status: 200,
                            push_endpoint: endpoint,
                        }
                    }
                    call::RegisterResponse::Error { error_msg, status, .. } => {
                        debug!("Got unregister fail, error: {}", error_msg);
                        ServerMessage::Register {
                            channel_id: channel_id,
                            status: status,
                            push_endpoint: "".into(),
                        }
                    }
                };
                let next_state = if self.data.unacked_messages() {
                    ClientState::WaitingForAcks
                } else {
                    ClientState::Await
                };
                ClientState::FinishSend(Some(msg), Some(Box::new(next_state)))
            }
            ClientState::WaitingForUnRegister(channel_id, ref mut response) => {
                debug!("State: WaitingForUnRegister");
                let msg = match try_ready!(response.poll()) {
                    call::UnRegisterResponse::Success { success } => {
                        debug!("Got the unregister response");
                        self.data.webpush.as_mut().unwrap().stats.unregisters += 1;
                        ServerMessage::Unregister {
                            channel_id: channel_id,
                            status: if success { 200 } else { 500 },
                        }
                    }
                    call::UnRegisterResponse::Error { error_msg, status, .. } => {
                        debug!("Got unregister fail, error: {}", error_msg);
                        ServerMessage::Unregister { channel_id, status }
                    }
                };
                let next_state = if self.data.unacked_messages() {
                    ClientState::WaitingForAcks
                } else {
                    ClientState::Await
                };
                ClientState::FinishSend(Some(msg), Some(Box::new(next_state)))
            }
            ClientState::WaitingForAcks => {
                debug!("State: WaitingForAcks");
                if let Some(next_state) = self.data.determine_acked_state() {
                    return Ok(next_state.into());
                }
                match try_ready!(self.data.input()) {
                    ClientMessage::Register { channel_id, key } => {
                        self.data.process_register(channel_id, key)
                    }
                    ClientMessage::Unregister { channel_id, code } => {
                        self.data.process_unregister(channel_id, code)
                    }
                    ClientMessage::Nack { .. } => {
                        self.data.srv.metrics.incr("ua.command.nack").ok();
                        self.data.webpush.as_mut().unwrap().stats.nacks += 1;
                        ClientState::WaitingForAcks
                    }
                    ClientMessage::Ack { updates } => self.data.process_acks(updates),
                    _ => return Err("Invalid state transition".into()),
                }
            }
            ClientState::WaitingForDelete(ref mut response) => {
                debug!("State: WaitingForDelete");
                try_ready!(response.poll());
                ClientState::WaitingForAcks
            }
            ClientState::WaitingForDropUser(ref mut response) => {
                debug!("State: WaitingForDropUser");
                try_ready!(response.poll());
                ClientState::Done
            }
            ClientState::Await => {
                debug!("State: Await");
                if self.data.webpush.as_ref().unwrap().flags.check {
                    return Ok(ClientState::CheckStorage.into());
                }
                match try_ready!(self.data.input_or_notif()) {
                    Either::A(ClientMessage::Register { channel_id, key }) => {
                        self.data.process_register(channel_id, key)
                    }
                    Either::A(ClientMessage::Unregister { channel_id, code }) => {
                        self.data.process_unregister(channel_id, code)
                    }
                    Either::A(ClientMessage::Nack { .. }) => {
                        self.data.srv.metrics.incr("ua.command.nack").ok();
                        self.data.webpush.as_mut().unwrap().stats.nacks += 1;
                        ClientState::WaitingForAcks
                    }
                    Either::B(ServerNotification::Notification(notif)) => {
                        let webpush = self.data.webpush.as_mut().unwrap();
                        webpush.unacked_direct_notifs.push(notif.clone());
                        debug!("Got a notification to send, sending!");
                        ClientState::FinishSend(
                            Some(ServerMessage::Notification(notif)),
                            Some(Box::new(ClientState::WaitingForAcks)),
                        )
                    }
                    Either::B(ServerNotification::CheckStorage) => {
                        let webpush = self.data.webpush.as_mut().unwrap();
                        webpush.flags.include_topic = true;
                        webpush.flags.check = true;
                        ClientState::Await
                    }
                    _ => return Err("Invalid message".into()),
                }
            }
            ClientState::ShutdownCleanup(ref mut err) => {
                debug!("State: ShutdownCleanup");
                if let Some(err_obj) = err.take() {
                    let mut error = err_obj.to_string();
                    for err in err_obj.iter().skip(1) {
                        error.push_str("\n");
                        error.push_str(&err.to_string());
                    }
                    debug!("Error for shutdown of {}: {}", host, error);
                };
                self.data.shutdown();
                ClientState::Done
            }
            ClientState::Done => {
                // We don't expect this to actually run, as this state will exit
                // the transition. Included for exhaustive matching.
                debug!("State: Done");
                ClientState::Done
            }
        };
        Ok(next_state.into())
    }
}

impl<T> ClientData<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    fn input(&mut self) -> Poll<ClientMessage, Error> {
        let item = match self.ws.poll()? {
            Async::Ready(None) => return Err("Client dropped".into()),
            Async::Ready(Some(msg)) => Async::Ready(msg),
            Async::NotReady => Async::NotReady,
        };
        Ok(item)
    }

    fn input_with_timeout(&mut self, timeout: &mut Timeout) -> Poll<ClientMessage, Error> {
        let item = match timeout.poll()? {
            Async::Ready(_) => return Err("Client timed out".into()),
            Async::NotReady => {
                match self.ws.poll()? {
                    Async::Ready(None) => return Err("Client dropped".into()),
                    Async::Ready(Some(msg)) => Async::Ready(msg),
                    Async::NotReady => Async::NotReady,
                }
            }
        };
        Ok(item)
    }

    fn input_or_notif(&mut self) -> Poll<Either<ClientMessage, ServerNotification>, Error> {
        let webpush = self.webpush.as_mut().unwrap();
        let item = match webpush.rx.poll() {
            Ok(Async::Ready(Some(notif))) => Either::B(notif),
            Ok(Async::Ready(None)) => return Err("Sending side dropped".into()),
            Ok(Async::NotReady) => {
                match self.ws.poll()? {
                    Async::Ready(None) => return Err("Client dropped".into()),
                    Async::Ready(Some(msg)) => Either::A(msg),
                    Async::NotReady => return Ok(Async::NotReady),
                }
            }
            Err(_) => return Err("Unexpected error".into()),
        };
        Ok(Async::Ready(item))
    }

    fn process_hello(
        &mut self,
        uaid: Uuid,
        message_month: String,
        reset_uaid: bool,
        rotate_message_table: bool,
        check_storage: bool,
        connected_at: u64,
    ) -> ClientState {
        let (tx, rx) = mpsc::unbounded();
        let mut flags = ClientFlags::new();
        flags.check = check_storage;
        flags.reset_uaid = reset_uaid;
        flags.rotate_message_table = rotate_message_table;

        self.webpush = Some(WebPushClient {
            uaid,
            flags,
            rx,
            message_month,
            unacked_direct_notifs: Vec::new(),
            unacked_stored_notifs: Vec::new(),
            unacked_stored_highest: None,
            connected_at,
            stats: SessionStatistics {
                uaid: uaid.hyphenated().to_string(),
                uaid_reset: reset_uaid,
                existing_uaid: check_storage,
                connection_type: String::from("webpush"),
                host: String::from("unknown"),
                direct_acked: 0,
                direct_storage: 0,
                stored_retrieved: 0,
                stored_acked: 0,
                nacks: 0,
                registers: 0,
                unregisters: 0,
            },
        });
        self.srv.connect_client(
            RegisteredClient { uaid: uaid, tx: tx },
        );
        let response = ServerMessage::Hello {
            uaid: uaid.hyphenated().to_string(),
            status: 200,
            use_webpush: Some(true),
        };
        ClientState::FinishSend(Some(response), Some(Box::new(ClientState::Await)))
    }

    fn process_register(&mut self, channel_id: Uuid, key: Option<String>) -> ClientState {
        debug!("Got a register command"; "channel_id" => channel_id.hyphenated().to_string());
        let webpush = self.webpush.as_ref().unwrap();
        let uaid = webpush.uaid.clone();
        let message_month = webpush.message_month.clone();
        let channel_id_str = channel_id.hyphenated().to_string();
        let fut = self.srv.register(
            uaid.simple().to_string(),
            message_month,
            channel_id_str,
            key,
        );
        ClientState::WaitingForRegister(channel_id, fut)
    }

    fn process_unregister(&mut self, channel_id: Uuid, code: Option<i32>) -> ClientState {
        debug!("Got a unregister command");
        let webpush = self.webpush.as_ref().unwrap();
        let uaid = webpush.uaid.clone();
        let message_month = webpush.message_month.clone();
        let channel_id_str = channel_id.hyphenated().to_string();
        let fut = self.srv.unregister(
            uaid.simple().to_string(),
            message_month,
            channel_id_str,
            code.unwrap_or(200),
        );
        ClientState::WaitingForUnRegister(channel_id, fut)
    }

    fn process_acks(&mut self, updates: Vec<ClientAck>) -> ClientState {
        self.srv.metrics.incr("ua.command.ack").ok();
        let webpush = self.webpush.as_mut().unwrap();
        let mut fut: Option<MyFuture<call::DeleteMessageResponse>> = None;
        for notif in updates.iter() {
            if let Some(pos) = webpush.unacked_direct_notifs.iter().position(|v| {
                v.channel_id == notif.channel_id && v.version == notif.version
            })
            {
                webpush.stats.direct_acked += 1;
                webpush.unacked_direct_notifs.remove(pos);
                continue;
            };
            if let Some(pos) = webpush.unacked_stored_notifs.iter().position(|v| {
                v.channel_id == notif.channel_id && v.version == notif.version
            })
            {
                webpush.stats.stored_acked += 1;
                let message_month = webpush.message_month.clone();
                let n = webpush.unacked_stored_notifs.remove(pos);
                if n.topic.is_some() {
                    if fut.is_none() {
                        fut = Some(self.srv.delete_message(message_month, n))
                    } else {
                        let my_fut = self.srv.delete_message(message_month, n);
                        fut = Some(Box::new(fut.take().unwrap().and_then(move |_| my_fut)));
                    }
                }
                continue;
            };
        }
        if let Some(my_fut) = fut {
            ClientState::WaitingForDelete(my_fut)
        } else {
            ClientState::WaitingForAcks
        }
    }

    // Called from WaitingForAcks to determine if we're in fact done waiting for acks
    // and to determine where we might go next
    fn determine_acked_state(&mut self) -> Option<ClientState> {
        let webpush = self.webpush.as_ref().unwrap();
        let all_acked = !self.unacked_messages();
        if all_acked && webpush.flags.check && webpush.flags.increment_storage {
            Some(ClientState::IncrementStorage)
        } else if all_acked && webpush.flags.check {
            Some(ClientState::CheckStorage)
        } else if all_acked && webpush.flags.rotate_message_table {
            Some(ClientState::WaitingForMigrateUser(self.srv.migrate_user(
                webpush.uaid.simple().to_string(),
                webpush.message_month.clone(),
            )))
        } else if all_acked && webpush.flags.reset_uaid {
            Some(ClientState::WaitingForDropUser(
                self.srv.drop_user(webpush.uaid.simple().to_string()),
            ))
        } else if all_acked && webpush.flags.none() {
            Some(ClientState::Await)
        } else {
            None
        }
    }

    fn unacked_messages(&self) -> bool {
        self.webpush.as_ref().unwrap().unacked_messages()
    }

    pub fn shutdown(&mut self) {
        // If we made it past hello, do more cleanup

        if self.webpush.is_some() {
            let webpush = self.webpush.take().unwrap();
            let now = time::precise_time_ns() / 1000;
            let elapsed = now - webpush.connected_at;
            // XXX: tags
            self.srv.metrics.time("ua.connection.lifespan", elapsed).ok();

            // If there's direct unack'd messages, they need to be saved out without blocking
            // here
            self.srv.disconnet_client(&webpush.uaid);
            let mut stats = webpush.stats.clone();
            let unacked_direct_notifs = webpush.unacked_direct_notifs.len();
            if unacked_direct_notifs > 0 {
                stats.direct_storage += unacked_direct_notifs as i32;
                self.srv.handle.spawn(
                    self.srv
                        .store_messages(
                            webpush.uaid.simple().to_string(),
                            webpush.message_month,
                            webpush.unacked_direct_notifs,
                        )
                        .then(|_| {
                            debug!("Finished saving unacked direct notifications");
                            Ok(())
                        }),
                )
            }

            // Parse the user-agent string
            let parser = Parser::new();
            let ua_result = parser.parse(self.user_agent.as_str());
            let mut ua_os_family = String::new();
            let mut ua_os_ver = String::new();
            let mut ua_browser_family = String::new();
            let mut ua_browser_ver = String::new();
            let mut ua_name = String::new();
            let mut ua_category = String::new();
            match ua_result {
                Some(WootheeResult { name, os, os_version, version, vendor, category, .. }) => {
                    ua_name = String::from(name);
                    ua_os_family = String::from(os);
                    ua_os_ver = os_version;
                    ua_browser_family = String::from(vendor);
                    ua_browser_ver = version;
                    ua_category = String::from(category);
                }
                None => ()
            };

            // Log out the final stats message
            info!("Session";
                "uaid_hash" => stats.uaid.as_str(),
                "uaid_reset" => stats.uaid_reset,
                "existing_uaid" => stats.existing_uaid,
                "connection_type" => stats.connection_type.as_str(),
                "host" => self.host.clone(),
                "ua_name" => ua_name.as_str(),
                "ua_os_family" => ua_os_family.as_str(),
                "ua_os_ver" => ua_os_ver.as_str(),
                "ua_browser_family" => ua_browser_family.as_str(),
                "ua_browser_ver" => ua_browser_ver.as_str(),
                "ua_category" => ua_category.as_str(),
                "connection_time" => elapsed,
                "direct_acked" => stats.direct_acked,
                "direct_storage" => stats.direct_storage,
                "stored_retrieved" => stats.stored_retrieved,
                "stored_acked" => stats.stored_acked,
                "nacks" => stats.nacks,
                "registers" => stats.registers,
                "unregisters" => stats.unregisters,
            );
        };
    }
}

impl<T> Future for Client<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Error> {
        loop {
            if let ClientState::Done = self.state {
                return Ok(().into());
            }
            match self.transition() {
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(next_state)) => self.state = next_state,
                Err(e) => self.state = ClientState::ShutdownCleanup(Some(e)),
            };
        }
    }
}
