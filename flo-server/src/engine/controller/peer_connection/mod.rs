mod system;

use std::fmt::Debug;
use std::net::SocketAddr;
use std::io;
use tokio_core::reactor::Handle;
use tokio_core::net::TcpStream;
use futures::Future;

use engine::{EngineRef, ConnectionId};
use engine::connection_handler::{create_connection_control_channels, ConnectionControlSender};
use event_loops::LoopHandles;
use flo_io::create_connection_handler;
use self::system::PeerConnectionImpl;

pub use self::system::PeerSystemConnection;

pub type ConnectionSendResult<T> = Result<(), T>;


/// Trait for creating outgoing connections (clever name, I know).
pub trait OutgoingConnectionCreator {
    fn establish_system_connection(&mut self, address: SocketAddr) -> Box<PeerSystemConnection>;
}

pub struct OutgoingConnectionCreatorImpl {
    event_loops: LoopHandles,
    engine_ref: EngineRef,
}

impl OutgoingConnectionCreatorImpl {
    pub fn new(loops: LoopHandles, engine: EngineRef) -> OutgoingConnectionCreatorImpl {
        OutgoingConnectionCreatorImpl {
            event_loops: loops,
            engine_ref: engine,
        }
    }
}

impl OutgoingConnectionCreator for OutgoingConnectionCreatorImpl {
    fn establish_system_connection(&mut self, address: SocketAddr) -> Box<PeerSystemConnection> {
        let OutgoingConnectionCreatorImpl { ref mut event_loops, ref engine_ref } = *self;

        let (sender, connection_id) = create_outgoing_connection(event_loops, address, engine_ref.clone());
        Box::new(PeerConnectionImpl::new(connection_id, sender))
    }
}


fn create_outgoing_connection(loops: &mut LoopHandles, client_addr: SocketAddr, engine_ref: EngineRef) -> (ConnectionControlSender, ConnectionId) {
    let connection_id = engine_ref.next_connection_id();
    let client_addr_copy = client_addr.clone();
    let mut system_stream = engine_ref.get_system_stream();

    let (control_tx, control_rx) = create_connection_control_channels();

    loops.next_handle().spawn( move |handle| {

        let owned_handle = handle.clone();
        let addr = client_addr_copy;
        TcpStream::connect(&addr, handle).map_err( move |io_err| {

            error!("Failed to create outgoing connection to address: {:?}: {:?}", addr, io_err);
            system_stream.outgoing_connection_failed(addr);

        }).and_then( move |tcp_stream| {

            create_connection_handler(owned_handle,
                                      engine_ref,
                                      connection_id,
                                      client_addr,
                                      tcp_stream,
                                      control_rx)
        })
    });

    (control_tx, connection_id)
}



