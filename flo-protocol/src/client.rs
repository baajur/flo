//! This is the wire protocol used to communicate between the server and client. Communication is done by sending and
//! receiving series' of distinct messages. Each message begins with an 8 byte header that identifies the type of message.
//! This is rather wasteful, but useful for the early stages when there's still a fair bit of debugging via manual inspection
//! of buffers. Messages are parsed using nom parser combinators, and serialized using simple a wrapper around a writer.
//!
//! The special cases in the protocol are for sending/receiving the events themselves. Since events can be quite large, they
//! are not actually implemented as a single message in the protocol, but rather as just a header. The header has all the basic
//! information as well as the length of the data portion (the body of the event). The event is read by first reading the
//! header and then reading however many bytes are indicated by the header for the body of the event.
//!
//! All numbers use big endian byte order.
//! All Strings are newline terminated.
use nom::{be_u64, be_u32, be_u16, IResult};
use event::{time, OwnedFloEvent, FloEvent, FloEventId, ActorId, Timestamp};
use serializer::Serializer;
use std::net::SocketAddr;
use std::io::{self, Read};
use std::fmt::Write;
use std::str::FromStr;
use std::sync::Arc;

pub mod headers {
    pub const CLIENT_AUTH: u8 = 1;
    pub const PRODUCE_EVENT: u8 = 2;
    pub const RECEIVE_EVENT: u8 = 3;
    pub const UPDATE_MARKER: u8 = 4;
    pub const START_CONSUMING: u8 = 5;
    pub const AWAITING_EVENTS: u8 = 6;
    pub const PEER_ANNOUNCE: u8 = 7;
    pub const PEER_UPDATE: u8 = 8;
    pub const ACK_HEADER: u8 = 9;
    pub const ERROR_HEADER: u8 = 10;
    pub const CLUSTER_STATE: u8 = 11;
    pub const SET_BATCH_SIZE: u8 = 12;
    pub const NEXT_BATCH: u8 = 13;
    pub const END_OF_BATCH: u8 = 14;
    pub const STOP_CONSUMING: u8 = 15;
    pub const CURSOR_CREATED: u8 = 16;
}

use self::headers::*;

pub const ERROR_INVALID_NAMESPACE: u8 = 15;
pub const ERROR_INVALID_CONSUMER_STATE: u8 = 16;
pub const ERROR_INVALID_VERSION_VECTOR: u8 = 17;
pub const ERROR_STORAGE_ENGINE_IO: u8 = 18;

/// Describes the type of error. This gets serialized a u8
#[derive(Debug, PartialEq, Clone)]
pub enum ErrorKind {
    /// Indicates that the namespace provided by a consumer was an invalid glob pattern
    InvalidNamespaceGlob,
    /// Indicates that the client connection was in an invalid state when it attempted some consumer operation
    InvalidConsumerState,
    /// Indicates that the provided version vector was invalid (contained more than one entry for at least one actor id)
    InvalidVersionVector,
    /// Unable to read or write to events file
    StorageEngineError,
}

/// Represents a response to any request that results in an error
#[derive(Debug, PartialEq, Clone)]
pub struct ErrorMessage {
    /// The op_id of the request to make it easier to correlate request/response pairs
    pub op_id: u32,

    /// The type of error
    pub kind: ErrorKind,

    /// A human-readable description of the error
    pub description: String,
}

impl ErrorKind {
    /// Converts from the serialized u8 to an ErrorKind
    pub fn from_u8(byte: u8) -> Result<ErrorKind, u8> {
        match byte {
            ERROR_INVALID_NAMESPACE => Ok(ErrorKind::InvalidNamespaceGlob),
            ERROR_INVALID_CONSUMER_STATE => Ok(ErrorKind::InvalidConsumerState),
            ERROR_INVALID_VERSION_VECTOR => Ok(ErrorKind::InvalidVersionVector),
            ERROR_STORAGE_ENGINE_IO => Ok(ErrorKind::StorageEngineError),
            other => Err(other)
        }
    }

    /// Converts the ErrorKind to it's serialized u8 value
    pub fn u8_value(&self) -> u8 {
        match self {
            &ErrorKind::InvalidNamespaceGlob => ERROR_INVALID_NAMESPACE,
            &ErrorKind::InvalidConsumerState => ERROR_INVALID_CONSUMER_STATE,
            &ErrorKind::InvalidVersionVector => ERROR_INVALID_VERSION_VECTOR,
            &ErrorKind::StorageEngineError => ERROR_STORAGE_ENGINE_IO,
        }
    }
}

/// The body of a ProduceEvent `ProtocolMessage`. This is sent from a client producer to the server, and the server will
/// respond with either an `EventAck` or an `ErrorMessage` to indicate success or failure respectively. Although the flo
/// protocol is pipelined, this message includes an `op_id` field to aid in correlation of requests and responses.
#[derive(Debug, PartialEq, Clone)]
pub struct ProduceEvent {
    /// This is an arbritrary number, assigned by the client, to aid in correlation of requests and responses. Clients may
    /// choose to just set it to the same value for every operation if they wish.
    pub op_id: u32,
    /// The namespace to produce the event to. See the `namespace` documentation on `FloEvent` for more information on
    /// namespaces in general. As far as the protocol is concerned, it's just serialized as a utf-8 string.
    pub namespace: String,
    /// The parent_id of the new event. This is typically set to the id of whatever event a consumer is responding to.
    /// The parent id is optional. On the wire, a null parent_id is serialized as an event id where both the counter and the
    /// actor are set to 0.
    pub parent_id: Option<FloEventId>,
    /// The event payload. As far as the flo server is concerned, this is just an opaque byte array. Note that events with
    /// 0-length bodies are perfectly fine.
    pub data: Vec<u8>,
}

/// Sent by the server to the producer of an event to acknowledge that the event was successfully persisted to the stream.
#[derive(Debug, PartialEq, Clone)]
pub struct EventAck {
    /// This will be set to the `op_id` that was sent in the `ProduceEventHeader`
    pub op_id: u32,

    /// The id that was assigned to the event. This id is immutable and must be the same across all servers in a flo cluster.
    pub event_id: FloEventId,
}

/// Sent by a client to the server to begin reading events from the stream.
#[derive(Debug, PartialEq, Clone)]
pub struct ConsumerStart {
    /// Operation id that is generated by the client and used to correlate the response with the request
    pub op_id: u32,

    /// The maximum number of events to consume. Set to `u64::MAX` if you want unlimited.
    pub max_events: u64,

    /// The namespace to consume from. This can be any valid glob pattern, to allow reading from multiple namespaces.
    pub namespace: String,
}


/// Represents information known about a member of the flo cluster from the perspective of whichever member sent the
/// ClusterState message.
#[derive(Debug, PartialEq, Clone)]
pub struct ClusterMember {
    /// the address of the cluster member. The peer should be reachable at this address without having to modify or fix it up
    pub addr: SocketAddr,

    /// The actor id of the peer
    pub actor_id: ActorId,

    /// Whether the peer is currently connected to the sender of the ClusterState message
    pub connected: bool,
}

/// Represents the known state of the cluster from the point of view of _one_ of it's members.
/// Keep in mind that each member of a given cluster may have a different record of what the state of the cluster is.
/// This message represents the point of view of the actor referred to by the `actor_id` field.
#[derive(Debug, PartialEq, Clone)]
pub struct ClusterState {
    /// The id of whichever actor has sent this message
    pub actor_id: ActorId,

    /// The port number that this actor is listening on. This is not a complete address because of the fact that it's not
    /// always possible for a server to know the correct address for connecting to itself.
    pub actor_port: u16,

    /// The current version vector of this actor
    pub version_vector: Vec<FloEventId>,

    /// Information on all the other known members of the cluster. This list will not include duplicated information about
    /// the actor who sent the message
    pub other_members: Vec<ClusterMember>,
}

/// Sent in a CursorCreated message from the server to a client to indicate that a cursor was successfully created.
/// Currently, this message only contains the batch size, but more fields may be added as they become necessary.
#[derive(Debug, PartialEq, Clone)]
pub struct CursorInfo {
    /// The operation id from the StartConsuming message that created this cursor.
    pub op_id: u32,

    /// The actual batch size that will be used by the server for sending events. Note that this value _may_ differ from the
    /// batch size that was explicitly set by the consumer, depending on server settings. This behavior is not currently
    /// implemented by the server, but it's definitely possible to change in the near future.
    pub batch_size: u32,
}

/// Used to be abstract over owned events versus shared references
#[derive(Debug, PartialEq, Clone)]
pub enum RecvEvent {
    Owned(OwnedFloEvent),
    Ref(Arc<OwnedFloEvent>)
}

impl RecvEvent {
    /// Converts into an OwnedFloEvent and avoids copying if this is already an `Owned` variant
    pub fn into_owned(self) -> OwnedFloEvent {
        match self {
            RecvEvent::Owned(owned) => owned,
            RecvEvent::Ref(arc) => <Arc<OwnedFloEvent> as FloEvent>::to_owned(&arc)
        }
    }
}

impl FloEvent for RecvEvent {
    fn id(&self) -> &FloEventId {
        match *self {
            RecvEvent::Owned(ref e) => e.id(),
            RecvEvent::Ref(ref e) => e.id()
        }
    }

    fn timestamp(&self) -> Timestamp {
        match *self {
            RecvEvent::Owned(ref e) => e.timestamp(),
            RecvEvent::Ref(ref e) => e.timestamp()
        }
    }

    fn parent_id(&self) -> Option<FloEventId> {
        match *self {
            RecvEvent::Owned(ref e) => e.parent_id(),
            RecvEvent::Ref(ref e) => e.parent_id()
        }
    }

    fn namespace(&self) -> &str {
        match *self {
            RecvEvent::Owned(ref e) => e.namespace(),
            RecvEvent::Ref(ref e) => e.namespace()
        }
    }

    fn data_len(&self) -> u32 {
        match *self {
            RecvEvent::Owned(ref e) => e.data_len(),
            RecvEvent::Ref(ref e) => e.data_len()
        }
    }

    fn data(&self) -> &[u8] {
        match *self {
            RecvEvent::Owned(ref e) => e.data(),
            RecvEvent::Ref(ref e) => e.data()
        }
    }

    fn to_owned(&self) -> OwnedFloEvent {
        match *self {
            RecvEvent::Owned(ref e) => <OwnedFloEvent as FloEvent>::to_owned(e),
            RecvEvent::Ref(ref e) => <Arc<OwnedFloEvent> as FloEvent>::to_owned(e)
        }
    }
}

/// Defines all the distinct messages that can be sent over the wire between client and server.
#[derive(Debug, PartialEq, Clone)]
pub enum ProtocolMessage {
    /// Signals a client's intent to publish a new event. The server will respond with either an `EventAck` or an `ErrorMessage`
    ProduceEvent(ProduceEvent),
    /// This is a complete event as serialized over the wire. This message is sent to to both consumers as well as other servers
    ReceiveEvent(RecvEvent),
    /// Sent from the server to client to acknowledge that an event was persisted successfully.
    AckEvent(EventAck),
    /// Sent by a client to set it's current position in the event stream
    UpdateMarker(FloEventId),
    /// sent by a client to start reading events from the stream
    StartConsuming(ConsumerStart),
    /// send by the server to a client in response to a StartConsuming message to indicate the start of a series of events
    CursorCreated(CursorInfo),
    /// sent by a client to a server to tell the server to stop sending events. This is required in order to reuse the connection for multiple queries
    StopConsuming,
    /// Sent by the client to set the batch size to use for consuming. It is an error to send this message while consuming.
    SetBatchSize(u32),
    /// Sent by the client to tell the server that it is ready for the next batch
    NextBatch,
    /// Sent by the server to notify a consumer that it has reached the end of a batch and that more events can be sent
    /// upon receipt of a `NextBatch` message by the server.
    EndOfBatch,
    /// Sent by the server to an active consumer to indicate that it has reached the end of the stream. The server will
    /// continue to send events as more come in, but this just lets the client know that it may be some time before more
    /// events are available. This message will only be sent at most once to a given consumer.
    AwaitingEvents,
    /// Sent between flo servers to announce their presence. Essentially makes a claim that the given server represents
    /// the given `ActorId` and provides whatever information the actor has about the current state of the cluster
    PeerAnnounce(ClusterState),
    /// Sent between flo servers to provide the version vector and cluster state of the peer
    PeerUpdate(ClusterState),
    /// This is just a bit of speculative engineering, honestly. Just don't even bother using it.
    ClientAuth {
        namespace: String,
        username: String,
        password: String,
    },
    /// Represents an error response to any other message
    Error(ErrorMessage),
}

named!{pub parse_str<String>,
    map_res!(
        length_data!(be_u16),
        |res| {
            ::std::str::from_utf8(res).map(|val| val.to_owned())
        }
    )
}

named!{parse_string_slice<&str>,
    map_res!(
        length_data!(be_u16),
        |res| {
            ::std::str::from_utf8(res)
        }
    )
}

named!{pub parse_auth<ProtocolMessage>,
    chain!(
        _tag: tag!(&[CLIENT_AUTH]) ~
        namespace: parse_str ~
        username: parse_str ~
        password: parse_str,
        || {
            ProtocolMessage::ClientAuth {
                namespace: namespace,
                username: username,
                password: password,
            }
        }
    )
}

fn require_event_id(id: Option<FloEventId>) -> Result<FloEventId, &'static str> {
    id.ok_or("EventId must not be all zeros")
}

named!{parse_non_zero_event_id<FloEventId>,
    map_res!(parse_event_id, require_event_id)
}

named!{pub parse_event_id<Option<FloEventId>>,
    chain!(
        counter: be_u64 ~
        actor: be_u16,
        || {
            if counter > 0 {
                Some(FloEventId::new(actor, counter))
            } else {
                None
            }
        }
    )
}

named!{pub parse_new_producer_event<ProtocolMessage>,
    chain!(
        _tag: tag!(&[PRODUCE_EVENT]) ~
        namespace: parse_str ~
        parent_id: parse_event_id ~
        op_id: be_u32 ~
        data_len: be_u32,
        || {
            ProtocolMessage::ProduceEvent(ProduceEvent{
                namespace: namespace.to_owned(),
                parent_id: parent_id,
                op_id: op_id,
                data: Vec::with_capacity(data_len as usize),
            })
        }
    )
}

named!{parse_timestamp<Timestamp>,
    map!(be_u64, time::from_millis_since_epoch)
}

named!{parse_receive_event_header<ProtocolMessage>,
    chain!(
        _tag: tag!(&[RECEIVE_EVENT]) ~
        id: parse_non_zero_event_id ~
        parent_id: parse_event_id ~
        timestamp: parse_timestamp ~
        namespace: parse_str ~
        data: length_data!(be_u32),
        || {
           ProtocolMessage::ReceiveEvent(RecvEvent::Owned(OwnedFloEvent {
                id: id,
                parent_id: parent_id,
                namespace: namespace,
                timestamp: timestamp,
                data: data.to_vec(),
            }))
        }
    )
}

named!{parse_event_ack<ProtocolMessage>,
    chain!(
        _tag: tag!(&[ACK_HEADER]) ~
        op_id: be_u32 ~
        counter: be_u64 ~
        actor_id: be_u16,
        || {
            ProtocolMessage::AckEvent(EventAck {
                op_id: op_id,
                event_id: FloEventId::new(actor_id, counter)
            })
        }
    )
}

named!{parse_update_marker<ProtocolMessage>,
    chain!(
        _tag: tag!(&[UPDATE_MARKER]) ~
        counter: be_u64 ~
        actor: be_u16,
        || {
            ProtocolMessage::UpdateMarker(
                FloEventId::new(actor, counter)
            )
        }
    )
}

named!{parse_start_consuming<ProtocolMessage>,
    chain!(
        _tag: tag!(&[START_CONSUMING]) ~
        op_id: be_u32 ~
        namespace: parse_str ~
        count: be_u64,
        || {
            ProtocolMessage::StartConsuming(ConsumerStart {
                op_id: op_id,
                namespace: namespace,
                max_events: count,
            })
        }
    )
}

named!{parse_cluster_state<ClusterState>,
    chain!(
        actor_id: be_u16 ~
        actor_port: be_u16 ~
        version_vec: parse_version_vec ~
        members: length_count!(be_u16, parse_cluster_member_status),
        || {
            ClusterState {
                actor_id: actor_id,
                actor_port: actor_port,
                version_vector: version_vec,
                other_members: members,
            }
        }
    )
}

named!{parse_socket_addr<SocketAddr>, map_res!(parse_string_slice, to_socket_addr) }

fn to_socket_addr(input: &str) -> Result<SocketAddr, ::std::net::AddrParseError> {
    SocketAddr::from_str(input)
}

fn to_bool(byte_slice: &[u8]) -> bool {
    byte_slice == &[1u8]
}

named!{parse_cluster_member_status<ClusterMember>,
    chain!(
        actor_id: be_u16 ~
        address: parse_socket_addr ~
        connected: map!(take!(1), to_bool),
        || {
            ClusterMember {
                addr: address,
                actor_id: actor_id,
                connected: connected,
            }
        }
    )
}

named!{parse_peer_announce<ProtocolMessage>,
    chain!(
        _tag: tag!(&[PEER_ANNOUNCE]) ~
        state: parse_cluster_state,
        || {
            ProtocolMessage::PeerAnnounce(state)
        }
    )
}

named!{parse_version_vec<Vec<FloEventId>>,
    length_count!(be_u16, parse_non_zero_event_id)
}

named!{parse_peer_update<ProtocolMessage>,
    chain!(
        _tag: tag!(&[PEER_UPDATE]) ~
        state: parse_cluster_state,
        || {
            ProtocolMessage::PeerUpdate(state)
        }
    )
}

named!{parse_error_message<ProtocolMessage>,
    chain!(
        _tag: tag!(&[ERROR_HEADER]) ~
        op_id: be_u32 ~
        kind: map_res!(take!(1), |res: &[u8]| {
            ErrorKind::from_u8(res[0])
        }) ~
        description: parse_str,
        || {
            ProtocolMessage::Error(ErrorMessage {
                op_id: op_id,
                kind: kind,
                description: description,
            })
        }
    )
}

named!{parse_awaiting_events<ProtocolMessage>, map!(tag!(&[AWAITING_EVENTS]), |_| {ProtocolMessage::AwaitingEvents})}

named!{parse_set_batch_size<ProtocolMessage>, chain!(
    _tag: tag!(&[SET_BATCH_SIZE]) ~
    batch_size: be_u32,
    || {
        ProtocolMessage::SetBatchSize(batch_size)
    }
)}

named!{parse_next_batch<ProtocolMessage>, map!(tag!(&[NEXT_BATCH]), |_| {ProtocolMessage::NextBatch})}
named!{parse_end_of_batch<ProtocolMessage>, map!(tag!(&[END_OF_BATCH]), |_| {ProtocolMessage::EndOfBatch})}
named!{parse_stop_consuming<ProtocolMessage>, map!(tag!(&[headers::STOP_CONSUMING]), |_| {ProtocolMessage::StopConsuming})}

named!{parse_cursor_created<ProtocolMessage>, chain!(
    _tag: tag!(&[headers::CURSOR_CREATED]) ~
    op_id: be_u32 ~
    batch_size: be_u32,
    || {
        ProtocolMessage::CursorCreated(CursorInfo{
            op_id: op_id,
            batch_size: batch_size
        })
    }
)}

named!{pub parse_any<ProtocolMessage>, alt!(
        parse_event_ack |
        parse_receive_event_header |
        parse_peer_update |
        parse_peer_announce |
        parse_update_marker |
        parse_start_consuming |
        parse_auth |
        parse_error_message |
        parse_awaiting_events |
        parse_new_producer_event |
        parse_set_batch_size |
        parse_next_batch |
        parse_end_of_batch |
        parse_stop_consuming |
        parse_cursor_created
)}

fn serialize_new_produce_header(header: &ProduceEvent, mut buf: &mut [u8]) -> usize {
    let (counter, actor) = header.parent_id.map(|id| {
        (id.event_counter, id.actor)
    }).unwrap_or((0, 0));

    Serializer::new(buf).write_u8(PRODUCE_EVENT)
                        .write_string(&header.namespace)
                        .write_u64(counter)
                        .write_u16(actor)
                        .write_u32(header.op_id)
                        .write_u32(header.data.len() as u32)
                        .finish()
}

fn serialize_event_ack(ack: &EventAck, buf: &mut [u8]) -> usize {
    Serializer::new(buf).write_u8(ACK_HEADER)
            .write_u32(ack.op_id)
            .write_u64(ack.event_id.event_counter)
            .write_u16(ack.event_id.actor)
            .finish()
}

fn serialize_error_message(err: &ErrorMessage, buf: &mut [u8]) -> usize {
    Serializer::new(buf).write_u8(ERROR_HEADER)
            .write_u32(err.op_id)
            .write_u8(err.kind.u8_value())
            .write_string(&err.description)
            .finish()
}

fn serialize_cluster_state(header: u8, state: &ClusterState, buf: &mut [u8]) -> usize {
    let mut addr_buffer = String::new();

    let mut ser = Serializer::new(buf).write_u8(header)
            .write_u16(state.actor_id)
            .write_u16(state.actor_port)
            .write_u16(state.version_vector.len() as u16);

    for id in state.version_vector.iter() {
        ser = ser.write_u64(id.event_counter).write_u16(id.actor);
    }

    ser = ser.write_u16(state.other_members.len() as u16);
    for member in state.other_members.iter() {
        addr_buffer.clear();
        write!(addr_buffer, "{}", member.addr).unwrap();

        ser = ser.write_u16(member.actor_id)
                 .write_string(&addr_buffer)
                 .write_bool(member.connected);
    }
    ser.finish()
}

fn serialize_receive_event_header(event: &RecvEvent, buf: &mut [u8]) -> usize {
    use event::FloEvent;

    Serializer::new(buf)
            .write_u8(::client::headers::RECEIVE_EVENT)
            .write_u64(event.id().event_counter)
            .write_u16(event.id().actor)
            .write_u64(event.parent_id().map(|id| id.event_counter).unwrap_or(0))
            .write_u16(event.parent_id().map(|id| id.actor).unwrap_or(0))
            .write_u64(time::millis_since_epoch(event.timestamp()))
            .write_string(event.namespace())
            .write_u32(event.data_len())
            .finish()
}

impl ProtocolMessage {

    pub fn serialize(&self, buf: &mut [u8]) -> usize {
        match *self {
            ProtocolMessage::ReceiveEvent(ref event) => {
                serialize_receive_event_header(event, buf)
            }
            ProtocolMessage::CursorCreated(ref info) => {
                Serializer::new(buf).write_u8(headers::CURSOR_CREATED)
                        .write_u32(info.op_id)
                        .write_u32(info.batch_size)
                        .finish()
            }
            ProtocolMessage::AwaitingEvents => {
                Serializer::new(buf).write_u8(AWAITING_EVENTS).finish()
            }
            ProtocolMessage::StopConsuming => {
                Serializer::new(buf).write_u8(headers::STOP_CONSUMING).finish()
            }
            ProtocolMessage::ProduceEvent(ref header) => {
                serialize_new_produce_header(header, buf)
            }
            ProtocolMessage::StartConsuming(ConsumerStart{ref op_id, ref namespace, ref max_events}) => {
                Serializer::new(buf).write_u8(START_CONSUMING)
                                    .write_u32(*op_id)
                                    .write_string(namespace)
                                    .write_u64(*max_events)
                                    .finish()
            }
            ProtocolMessage::UpdateMarker(id) => {
                Serializer::new(buf).write_u8(UPDATE_MARKER)
                                    .write_u64(id.event_counter)
                                    .write_u16(id.actor)
                                    .finish()
            }
            ProtocolMessage::ClientAuth {ref namespace, ref username, ref password} => {
                Serializer::new(buf).write_u8(CLIENT_AUTH)
                                    .write_string(namespace)
                                    .write_string(username)
                                    .write_string(password)
                                    .finish()
            }
            ProtocolMessage::PeerUpdate(ref state) => {
                serialize_cluster_state(PEER_UPDATE, state, buf)
            }
            ProtocolMessage::PeerAnnounce(ref cluster_state) => {
                serialize_cluster_state(PEER_ANNOUNCE, cluster_state, buf)
            }
            ProtocolMessage::AckEvent(ref ack) => {
                serialize_event_ack(ack, buf)
            }
            ProtocolMessage::Error(ref err_message) => {
                serialize_error_message(err_message, buf)
            }
            ProtocolMessage::SetBatchSize(batch_size) => {
                Serializer::new(buf).write_u8(SET_BATCH_SIZE)
                                    .write_u32(batch_size)
                                    .finish()
            }
            ProtocolMessage::NextBatch => {
                buf[0] = NEXT_BATCH;
                1
            }
            ProtocolMessage::EndOfBatch => {
                buf[0] = END_OF_BATCH;
                1
            }
        }
    }

    pub fn get_body(&self) -> Option<&Vec<u8>> {
        match *self {
            ProtocolMessage::ProduceEvent(ref produce) => {
                Some(&produce.data)
            }
            ProtocolMessage::ReceiveEvent(ref event) => {
                let data = match *event {
                    RecvEvent::Owned(ref owned) => &owned.data,
                    RecvEvent::Ref(ref arc) => &arc.data
                };
                Some(data)
            }
            _ => None
        }
    }

    pub fn get_op_id(&self) -> u32 {
        match *self {
            ProtocolMessage::ProduceEvent(ref prod) => prod.op_id,
            ProtocolMessage::StartConsuming(ref start) => start.op_id,
            ProtocolMessage::CursorCreated(ref info) => info.op_id,
            ProtocolMessage::Error(ref err) => err.op_id,
            ProtocolMessage::AckEvent(ref ack) => ack.op_id,
            _ => 0
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Read;
    use nom::{IResult, Needed};
    use event::{OwnedFloEvent, time, FloEventId};
    use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};

    fn test_serialize_then_deserialize(message: &ProtocolMessage) {
        let result  = ser_de(message);
        assert_eq!(*message, result);
    }

    fn ser_de(message: &ProtocolMessage) -> ProtocolMessage {
        serde_with_body(message, false)
    }

    fn serde_with_body(message: &ProtocolMessage, include_body: bool) -> ProtocolMessage {
        let mut buffer = [0; 1024];

        let mut len = message.serialize(&mut buffer[..]);
        if include_body {
            if let Some(body) = message.get_body() {
                (&mut buffer[len..(len + body.len())]).copy_from_slice(body);
                len += body.len();
            }
        }
        (&mut buffer[len..(len + 4)]).copy_from_slice(&[4, 3, 2, 1]); // extra bytes at the end of the buffer
        println!("buffer: {:?}", &buffer[..(len + 4)]);

        match parse_any(&buffer) {
            IResult::Done(remaining, result) => {
                assert!(remaining.starts_with(&[4, 3, 2, 1]));
                result
            }
            IResult::Error(err) => {
                panic!("Got parse error: {:?}", err)
            }
            IResult::Incomplete(need) => {
                panic!("Got incomplete: {:?}", need)
            }
        }

    }

    #[test]
    fn serde_receive_event() {
        let event = OwnedFloEvent {
            id: FloEventId::new(4, 5),
            timestamp: time::from_millis_since_epoch(99),
            parent_id: Some(FloEventId::new(4, 3)),
            namespace: "/foo/bar".to_owned(),
            data: vec![9; 99],
        };
        let message = ProtocolMessage::ReceiveEvent(RecvEvent::Owned(event.clone()));
        let result = serde_with_body(&message, true);
        assert_eq!(message, result);

        let arc_message = ProtocolMessage::ReceiveEvent(RecvEvent::Ref(Arc::new(event.clone())));
        let result = serde_with_body(&arc_message, true);
        assert_eq!(message, result);
    }

    #[test]
    fn stop_consuming_is_serialized_and_parsed() {
        test_serialize_then_deserialize(&ProtocolMessage::StopConsuming);
    }

    #[test]
    fn cursor_created_is_serialized_and_parsed() {
        test_serialize_then_deserialize(&ProtocolMessage::CursorCreated(CursorInfo{op_id: 543, batch_size: 78910}));
    }

    #[test]
    fn next_batch_is_serialized_and_parsed() {
        test_serialize_then_deserialize(&ProtocolMessage::NextBatch);
    }

    #[test]
    fn end_of_batch_is_serialized_and_parsed() {
        test_serialize_then_deserialize(&ProtocolMessage::EndOfBatch);
    }

    #[test]
    fn set_batch_size_is_serialized_and_parsed() {
        test_serialize_then_deserialize(&ProtocolMessage::SetBatchSize(1234567));
    }

    #[test]
    fn awaiting_events_message_is_serialized_and_parsed() {
        test_serialize_then_deserialize(&mut ProtocolMessage::AwaitingEvents);
    }

    #[test]
    fn error_message_is_parsed() {
        let error = ErrorMessage {
            op_id: 12345,
            kind: ErrorKind::InvalidNamespaceGlob,
            description: "some shit happened".to_owned(),
        };
        test_serialize_then_deserialize(&mut ProtocolMessage::Error(error));
    }

    #[test]
    fn acknowledge_event_message_is_parsed() {
        test_serialize_then_deserialize(&mut ProtocolMessage::AckEvent(EventAck{
            op_id: 2345667,
            event_id: FloEventId::new(123, 456),
        }));
    }

    #[test]
    fn peer_announce_is_parsed() {
        let state = ClusterState {
            actor_id: 5,
            actor_port: 5555,
            version_vector: vec![FloEventId::new(5, 6), FloEventId::new(1, 9), FloEventId::new(2, 1)],
            other_members: vec![
                ClusterMember {
                    addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0,0,0,0), 4444)),
                    actor_id: 6,
                    connected: true,
                },
                ClusterMember {
                    addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(7, 8, 9, 10), 3333)),
                    actor_id: 3,
                    connected: false,
                },
                ClusterMember {
                    addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0,0,0,0), 4444)),
                    actor_id: 2,
                    connected: true,
                },
            ],
        };
        test_serialize_then_deserialize(&mut ProtocolMessage::PeerAnnounce(state));
    }

    #[test]
    fn peer_update_is_parsed() {
        let state = ClusterState {
            actor_id: 5,
            actor_port: 5555,
            version_vector: vec![FloEventId::new(5, 6), FloEventId::new(1, 9), FloEventId::new(2, 1)],
            other_members: vec![
                ClusterMember {
                    addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0,0,0,0), 4444)),
                    actor_id: 6,
                    connected: true,
                },
                ClusterMember {
                    addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(7, 8, 9, 10), 3333)),
                    actor_id: 3,
                    connected: false,
                },
                ClusterMember {
                    addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0,0,0,0), 4444)),
                    actor_id: 2,
                    connected: true,
                },
            ],
        };
        test_serialize_then_deserialize(&mut ProtocolMessage::PeerUpdate(state));
    }

    #[test]
    fn event_marker_update_is_parsed() {
        test_serialize_then_deserialize(&mut ProtocolMessage::UpdateMarker(FloEventId::new(2, 255)));
    }

    #[test]
    fn start_consuming_message_is_parsed() {
        test_serialize_then_deserialize(&mut ProtocolMessage::StartConsuming(ConsumerStart{
            op_id: 123,
            namespace: "/test/ns".to_owned(),
            max_events: 8766
        }));
    }

    #[test]
    fn parse_producer_event_parses_the_header_but_not_the_data() {
        let input = ProduceEvent {
            namespace: "/the/namespace".to_owned(),
            parent_id: Some(FloEventId::new(123, 456)),
            op_id: 9,
            data: vec![9; 5]
        };
        let mut message_input = ProtocolMessage::ProduceEvent(input.clone());
        let message_result = ser_de(&mut message_input);

        if let ProtocolMessage::ProduceEvent(result) = message_result {
            assert_eq!(input.namespace, result.namespace);
            assert_eq!(input.parent_id, result.parent_id);
            assert_eq!(input.op_id, result.op_id);

            // The vector must be allocated with the correct capacity, but we haven't actually read all the data
            assert_eq!(input.data.len(), result.data.capacity());
        } else {
            panic!("got the wrong fucking message. Just quit now");
        }
    }


    #[test]
    fn parse_client_auth_returns_incomplete_result_when_password_is_missing() {
        let mut input = vec![headers::CLIENT_AUTH];
        input.extend_from_slice(b"hello\n");
        input.extend_from_slice(b"world\n");

        let result = parse_auth(&input);
        match result {
            IResult::Incomplete(_) => { }
            e @ _ => panic!("Expected Incomplete, got: {:?}", e)
        }
    }

    #[test]
    fn parse_client_auth_parses_valid_header_with_no_remaining_bytes() {
        test_serialize_then_deserialize(&mut ProtocolMessage::ClientAuth {
            namespace: "hello".to_owned(),
            username: "usr".to_owned(),
            password: "pass".to_owned(),
        });
    }

    #[test]
    fn parse_client_auth_returns_error_result_when_namespace_contains_invalid_utf_characters() {
        let mut input = Vec::new();
        input.extend_from_slice(b"FLO_AUT\n");
        input.extend_from_slice(&vec![0, 0xC0, 0, 0, 2, 10]);
        input.extend_from_slice(b"usr\n");
        input.extend_from_slice(b"pass\n");
        let result = parse_auth(&input);
        assert!(result.is_err());
    }


    #[test]
    fn parse_string_returns_empty_string_string_length_is_0() {
        let input = vec![0, 0, 110, 4, 5, 6, 7];
        let (remaining, result) = parse_str(&input).unwrap();
        assert_eq!("".to_owned(), result);
        assert_eq!(&vec![110, 4, 5, 6, 7], &remaining);
    }

    #[test]
    fn string_is_serialized_and_parsed() {
        let input = "hello\n\tmoar bytes";
        let mut buffer = [0; 64];

        let n_bytes = Serializer::new(&mut buffer).write_string(input).finish();
        assert_eq!(19, n_bytes);

        let (_, result) = parse_str(&buffer[0..19]).unwrap();
        assert_eq!(input.to_owned(), result);
    }

    #[test]
    fn this_works_how_i_think_it_does() {
        let input = vec![
            3,
            0, 0, 0, 0, 0, 0, 1, 34,  0, 1,
            0, 0, 0, 0, 0, 0, 0, 0,   0, 0,
            0, 0, 1, 93, 77, 45, 214, 26,
            47, 101, 118, 101
        ];

        let result = parse_any(&input);
        let expected = IResult::Incomplete(Needed::Size(12164));
        assert_eq!(expected, result);
    }
}
