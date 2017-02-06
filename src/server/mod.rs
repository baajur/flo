pub mod engine;
pub mod metrics;
mod flo_io;
mod channel_sender;
mod event_loops;

use futures::stream::Stream;
use tokio_core::net::{TcpStream, TcpListener};

use self::channel_sender::ChannelSender;
use server::engine::BackendChannels;
use self::engine::api::next_connection_id;

use futures::sync::mpsc::unbounded;
use std::path::PathBuf;
use std::net::{SocketAddr, Ipv4Addr, SocketAddrV4};

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MemoryUnit {
    Megabyte,
    Kilobyte,
    Byte
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub struct MemoryLimit {
    amount: usize,
    unit: MemoryUnit,
}


impl MemoryLimit {
    pub fn new(amount: usize, unit: MemoryUnit) -> MemoryLimit {
        MemoryLimit {
            amount: amount,
            unit: unit,
        }
    }

    pub fn as_bytes(&self) -> usize {
        let multiplier = match self.unit {
            MemoryUnit::Byte => 1,
            MemoryUnit::Kilobyte => 1024,
            MemoryUnit::Megabyte => 1024 * 1024,
        };
        multiplier * self.amount
    }
}

#[derive(PartialEq, Clone)]
pub struct ServerOptions {
    pub port: u16,
    pub data_dir: PathBuf,
    pub default_namespace: String,
    pub max_events: usize,
    pub max_cached_events: usize,
    pub max_cache_memory: MemoryLimit,
    pub cluster_addresses: Option<Vec<SocketAddr>>,
}



pub fn run(options: ServerOptions) {
    let (join_handle, event_loop_handles) = self::event_loops::spawn_default_event_loops().unwrap();

    let server_port = options.port;
    let address: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), server_port));

    let (cluster_tx, cluster_rx) = unbounded();

    let BackendChannels{producer_manager, consumer_manager} = engine::run(options, cluster_tx);

    let mut client_loop_handles = event_loop_handles.clone();
    let channel_sender = ChannelSender {
        producer_manager: producer_manager.clone(),
        consumer_manager: consumer_manager.clone(),
    };
    client_loop_handles.next_handle().spawn(move |handle| {
        let listener = TcpListener::bind(&address, &handle).unwrap();

        info!("Started listening on port: {}", server_port);

        listener.incoming().map_err(|io_err| {
            error!("Error creating new connection: {:?}", io_err);
        }).for_each(move |(tcp_stream, client_addr): (TcpStream, SocketAddr)| {

            let connection_id = next_connection_id();
            let remote_handle = client_loop_handles.next_handle();
            flo_io::setup_message_streams(connection_id, tcp_stream, client_addr, channel_sender.clone(), &remote_handle);
            Ok(())
        })
    });

    let engine_sender = ChannelSender {
        producer_manager: producer_manager.clone(),
        consumer_manager: consumer_manager.clone(),
    };
    flo_io::start_cluster_io(cluster_rx, event_loop_handles, engine_sender);

    join_handle.join();
}

