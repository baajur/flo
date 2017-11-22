pub mod event_stream;

mod controller;
mod connection_handler;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize};

use protocol::ProtocolMessage;
use event::OwnedFloEvent;
use self::event_stream::EventStreamRef;

pub use self::controller::{ControllerOptions, SystemStreamRef, start_controller};
pub use self::connection_handler::{ConnectionHandler, ConnectionHandlerResult};

pub type ConnectionId = usize;

use engine::event_stream::partition::PersistentEvent;

/// Thy type of messages that are received from clients
pub type ReceivedProtocolMessage = ProtocolMessage<OwnedFloEvent>;
/// The type of messages that are sent to client
pub type SendProtocolMessage = ProtocolMessage<PersistentEvent>;

pub type ClientSender = ::futures::sync::mpsc::UnboundedSender<SendProtocolMessage>;
pub type ClientReceiver = ::futures::sync::mpsc::UnboundedReceiver<SendProtocolMessage>;

pub fn create_client_channels() -> (ClientSender, ClientReceiver) {
    ::futures::sync::mpsc::unbounded()
}


pub static SYSTEM_STREAM_NAME: &'static str = "system";

pub fn system_stream_name() -> String {
    SYSTEM_STREAM_NAME.to_owned()
}

#[derive(Clone, Debug)]
pub struct EngineRef {
    current_connection_id: Arc<AtomicUsize>,
    system_stream: SystemStreamRef,
    event_streams: Arc<Mutex<HashMap<String, EventStreamRef>>>
}

#[derive(Debug)]
pub enum ConnectError {
    InitFailed(::std::io::Error),
    NoStream,
}

impl EngineRef {
    pub fn new(system_stream: SystemStreamRef, event_streams: Arc<Mutex<HashMap<String, EventStreamRef>>>) -> EngineRef {
        EngineRef {
            current_connection_id: Arc::new(AtomicUsize::new(0)),
            system_stream,
            event_streams
        }
    }

    pub fn next_connection_id(&self) -> ConnectionId {
        let old = self.current_connection_id.fetch_add(1, ::std::sync::atomic::Ordering::SeqCst);
        old + 1
    }

    pub fn get_stream(&self, stream_name: &str) -> Result<EventStreamRef, ConnectError> {
        let streams = self.event_streams.lock().unwrap();
        if let Some(stream) = streams.get(stream_name).map(|s| s.clone()) {
            Ok(stream)
        } else {
            Err(ConnectError::NoStream)
        }
    }

    pub fn get_default_stream(&self) -> EventStreamRef {
        let stream = {
            let guard = self.event_streams.lock().unwrap();
            guard.values().next().map(|stream| stream.clone())
        };
        stream.unwrap_or_else(|| {
            self.system_stream.to_event_stream()
        })
    }
}



