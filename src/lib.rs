#![cfg_attr(feature = "nightly", feature(alloc_system))]
#![cfg_attr(feature = "nightly", feature(proc_macro))]
#![cfg_attr(
    feature = "cargo-clippy",
    allow(
        large_enum_variant,
        new_without_default_derive,
        expect_fun_call,
        or_fun_call,
        useless_format,
        cyclomatic_complexity,
        needless_pass_by_value,
        module_inception,
        match_bool
    )
)]

#[cfg(feature = "nightly")]
extern crate alloc_system;
extern crate clap;
extern crate env_logger;
#[macro_use]
extern crate log;
#[macro_use]
extern crate failure;
extern crate chrono;
extern crate crossbeam;
extern crate crypto;
extern crate num_bigint;
extern crate num_traits;
#[macro_use]
extern crate futures;
extern crate byteorder;
extern crate bytes;
extern crate rand;
extern crate tokio;
extern crate tokio_util;
extern crate uuid;
#[macro_use]
extern crate serde_derive;
extern crate bincode;
extern crate clear_on_drop;
pub extern crate hbbft;
extern crate parking_lot;
extern crate serde;
extern crate serde_bytes;
extern crate tokio_serde_bincode;

#[cfg(feature = "nightly")]
use alloc_system::System;

#[cfg(feature = "nightly")]
#[global_allocator]
static A: System = System;

pub mod hydrabadger;
pub mod peer;

use bytes::{Bytes, BytesMut};
use hbbft::{
    crypto::{PublicKey, PublicKeySet, SecretKey, Signature},
    dynamic_honey_badger::{
        Change as DhbChange, DynamicHoneyBadger, JoinPlan, Message as DhbMessage,
    },
    sync_key_gen::{Ack, Part},
    Contribution as HbbftContribution, CpStep as MessagingStep, NodeIdT,
};
use rand::{
    distributions::{Distribution, Standard},
    Rng,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::BTreeMap,
    fmt::{self, Debug, Display},
    marker::PhantomData,
    net::SocketAddr,
    ops::Deref,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
};
use tokio_util::codec::{Framed, length_delimited::LengthDelimitedCodec, Encoder, BytesCodec};

use futures::stream::Stream;
use futures::sink::Sink;
use std::pin::Pin;
use std::task::{Context, Poll};
use futures::{channel::mpsc, SinkExt};

use uuid::Uuid;
use std::convert::AsRef;
pub use crate::hydrabadger::{Config, Hydrabadger};
// TODO: Create a separate, library-wide error type.
pub use crate::hydrabadger::key_gen;
pub use crate::hydrabadger::Error;
pub use crate::hydrabadger::StateDsct;
pub use hbbft::dynamic_honey_badger::Batch;

//TODO: 后续需要对这里所有的管道大小进行限制
/// sender of wire message channel.
type WireTx<C, N> = mpsc::UnboundedSender<WireMessage<C, N>>;

/// receiver of wire message channel.
type WireRx<C, N> = mpsc::UnboundedReceiver<WireMessage<C, N>>;

/// sender of internal message channel.
type InternalTx<C, N> = mpsc::UnboundedSender<InternalMessage<C, N>>;

/// receiver of internal message channel.
type InternalRx<C, N> = mpsc::UnboundedReceiver<InternalMessage<C, N>>;

/// sender of batch output channel.
type BatchTx<C, N> = mpsc::UnboundedSender<Batch<C, N>>;

/// receiver of batch output channel.
pub type BatchRx<C, N> = mpsc::UnboundedReceiver<Batch<C, N>>;

/// sender of epoch number output channel.
type EpochTx = mpsc::UnboundedSender<u64>;

/// receiver of epoch number output channel.
pub type EpochRx = mpsc::UnboundedReceiver<u64>;

pub trait Contribution:
    HbbftContribution + Clone + Debug + Serialize + DeserializeOwned + 'static
{
}

impl<C> Contribution for C where
    C: HbbftContribution + Clone + Debug + Serialize + DeserializeOwned + 'static
{
}

/// A transaction.
#[derive(Serialize, Deserialize, Eq, PartialEq, Hash, Ord, PartialOrd, Debug, Clone)]
pub struct Transaction(pub Vec<u8>);

impl Transaction {
    pub fn random(len: usize) -> Transaction {
        Transaction(
            rand::thread_rng()
                .sample_iter(&Standard)
                .take(len)
                .collect(),
        )
    }
}

pub trait NodeId: NodeIdT + Serialize + DeserializeOwned + Display + 'static {}

impl<N> NodeId for N where N: NodeIdT + Serialize + DeserializeOwned + Display + 'static {}

/// A unique identifier.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Uid(pub(crate) Uuid);

impl Uid {
    /// Returns a new, random `Uid`.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for Uid {
    fn default() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Distribution<Uid> for Standard {
    fn sample<R: Rng + ?Sized>(&self, _rng: &mut R) -> Uid {
        Uid::new()
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

impl fmt::Debug for Uid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

pub type Message<N> = DhbMessage<N>;
pub type Step<C, N> = MessagingStep<DynamicHoneyBadger<C, N>>;
pub type Change<N> = DhbChange<N>;

/// A peer's incoming (listening) address.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct InAddr(pub SocketAddr);

impl Deref for InAddr {
    type Target = SocketAddr;
    fn deref(&self) -> &SocketAddr {
        &self.0
    }
}

impl fmt::Display for InAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "InAddr({})", self.0)
    }
}

/// An internal address used to respond to a connected peer.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct OutAddr(pub SocketAddr);

impl Deref for OutAddr {
    type Target = SocketAddr;
    fn deref(&self) -> &SocketAddr {
        &self.0
    }
}

impl fmt::Display for OutAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "OutAddr({})", self.0)
    }
}

/// Nodes of the network.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkNodeInfo<N> {
    pub(crate) nid: N,
    pub(crate) in_addr: InAddr,
    pub(crate) pk: PublicKey,
}

type ActiveNetworkInfo<N> = (
    Vec<NetworkNodeInfo<N>>,
    PublicKeySet,
    BTreeMap<N, PublicKey>,
);

/// The current state of the network.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NetworkState<N: Ord> {
    None,
    Unknown(Vec<NetworkNodeInfo<N>>),
    AwaitingMorePeersForKeyGeneration(Vec<NetworkNodeInfo<N>>),
    GeneratingKeys(Vec<NetworkNodeInfo<N>>, BTreeMap<N, PublicKey>),
    Active(ActiveNetworkInfo<N>),
}

/// Messages sent over the network between nodes.
///
/// [`Message`](enum.WireMessageKind.html#variant.Message) variants are among
/// those verified.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WireMessageKind<C, N: Ord> {
    HelloFromValidator(N, InAddr, PublicKey, NetworkState<N>),
    HelloRequestChangeAdd(N, InAddr, PublicKey),
    WelcomeReceivedChangeAdd(N, PublicKey, NetworkState<N>),
    RequestNetworkState,
    NetworkState(NetworkState<N>),
    Goodbye,
    #[serde(with = "serde_bytes")]
    // TODO(c0gent): Remove.
    Bytes(Bytes),
    /// A Honey Badger message.
    ///
    /// All received messages are verified against the senders public key
    /// using an attached signature.
    Message(N, Message<N>),
    // TODO(c0gent): Remove.
    Transaction(N, C),
    /// Messages used during synchronous key generation.
    KeyGen(key_gen::InstanceId, key_gen::Message),
    JoinPlan(JoinPlan<N>),
}

/// Messages sent over the network between nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireMessage<C, N: Ord> {
    kind: WireMessageKind<C, N>,
}

impl<C: Contribution, N: NodeId> WireMessage<C, N> {
    pub fn hello_from_validator(
        src_uid: N,
        in_addr: InAddr,
        pk: PublicKey,
        net_state: NetworkState<N>,
    ) -> WireMessage<C, N> {
        WireMessageKind::HelloFromValidator(src_uid, in_addr, pk, net_state).into()
    }

    /// Returns a `HelloRequestChangeAdd` variant.
    pub fn hello_request_change_add(
        src_uid: N,
        in_addr: InAddr,
        pk: PublicKey,
    ) -> WireMessage<C, N> {
        WireMessageKind::HelloRequestChangeAdd(src_uid, in_addr, pk).into()
    }

    /// Returns a `WelcomeReceivedChangeAdd` variant.
    pub fn welcome_received_change_add(
        src_uid: N,
        pk: PublicKey,
        net_state: NetworkState<N>,
    ) -> WireMessage<C, N> {
        WireMessageKind::WelcomeReceivedChangeAdd(src_uid, pk, net_state).into()
    }

    /// Returns an `Input` variant.
    pub fn transaction(src_uid: N, txn: C) -> WireMessage<C, N> {
        WireMessageKind::Transaction(src_uid, txn).into()
    }

    /// Returns a `Message` variant.
    pub fn message(src_uid: N, msg: Message<N>) -> WireMessage<C, N> {
        WireMessageKind::Message(src_uid, msg).into()
    }

    pub fn key_gen(instance_id: key_gen::InstanceId, msg: key_gen::Message) -> WireMessage<C, N> {
        WireMessageKind::KeyGen(instance_id, msg).into()
    }

    pub fn key_gen_part(instance_id: key_gen::InstanceId, part: Part) -> WireMessage<C, N> {
        WireMessage::key_gen(instance_id, key_gen::Message::part(part))
    }

    pub fn key_gen_ack(instance_id: key_gen::InstanceId, ack: Ack) -> WireMessage<C, N> {
        WireMessage::key_gen(instance_id, key_gen::Message::ack(ack))
    }

    pub fn join_plan(jp: JoinPlan<N>) -> WireMessage<C, N> {
        WireMessageKind::JoinPlan(jp).into()
    }

    /// Returns the wire message kind.
    pub fn kind(&self) -> &WireMessageKind<C, N> {
        &self.kind
    }

    /// Consumes this `WireMessage` into its kind.
    pub fn into_kind(self) -> WireMessageKind<C, N> {
        self.kind
    }
}

impl<C: Contribution, N: NodeId> From<WireMessageKind<C, N>> for WireMessage<C, N> {
    fn from(kind: WireMessageKind<C, N>) -> WireMessage<C, N> {
        WireMessage { kind }
    }
}

/// A serialized `WireMessage` signed by the sender.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedWireMessage {
    message: Vec<u8>,
    sig: Signature,
}

/// A stream/sink of `WireMessage`s connected to a socket.
pub struct WireMessages<C: Contribution + Unpin, N: NodeId + Unpin> {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
    local_sk: SecretKey,
    peer_pk: Option<PublicKey>,
    _c: PhantomData<C>,
    _n: PhantomData<N>,
}

impl<C: Contribution + Unpin, N: NodeId + DeserializeOwned + Unpin> WireMessages<C, N> {
    pub fn new(socket: TcpStream, local_sk: SecretKey) -> WireMessages<C, N> {
        WireMessages {
            framed: Framed::new(socket, LengthDelimitedCodec::new()),
            local_sk,
            peer_pk: None,
            _c: PhantomData,
            _n: PhantomData,
        }
    }

    pub fn set_peer_public_key(&mut self, peer_pk: PublicKey) {
        assert!(self.peer_pk.map(|pk| pk == peer_pk).unwrap_or(true));
        self.peer_pk = Some(peer_pk);
    }

    pub fn socket(&self) -> &TcpStream {
        self.framed.get_ref()
    }

    pub async fn send_msg(&mut self, msg: WireMessage<C, N>) -> Result<(), Error> {
        let message = bincode::serialize(&msg).map_err(Error::Serde)?;
        let sig = self.local_sk.sign(&message);

        let signed_message = SignedWireMessage { message, sig };
        let serialized_message = bincode::serialize(&signed_message)
            .map_err(|err| Error::Io(std::io::Error::new(std::io::ErrorKind::Other, err)))?;

        self.framed.send(serialized_message.into()).await?;
        Ok(())
    }
}


impl<C: Contribution + Unpin, N: NodeId + DeserializeOwned + Unpin> Stream for WireMessages<C, N> {
    type Item = Result<WireMessage<C, N>, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.framed).poll_next(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                let s_msg: SignedWireMessage =
                    bincode::deserialize(&frame.freeze()).map_err(Error::Serde)?;
                let msg: WireMessage<C, N> =
                    bincode::deserialize(&s_msg.message).map_err(Error::Serde)?;

                // Verify signature for certain variants.
                match msg.kind {
                    WireMessageKind::Message(..) | WireMessageKind::KeyGen(..) => {
                        let peer_pk = this
                            .peer_pk
                            .ok_or(Error::VerificationMessageReceivedUnknownPeer)?;
                        if !peer_pk.verify(&s_msg.sig, &s_msg.message) {
                            return Poll::Ready(Some(Err(Error::InvalidSignature)));
                        }
                    }
                    _ => {}
                }
                Poll::Ready(Some(Ok(msg)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
/* 
impl<C: Contribution, N: NodeId + Serialize> Sink<WireMessage<C, N>> for WireMessages<C, N> {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        Pin::new(&mut this.framed).poll_ready(cx).map_err(Error::from)
    }

    fn start_send(self: Pin<&mut Self>, item: WireMessage<C, N>) -> Result<(), Self::Error> {
        // TODO: Reuse buffer:
        let mut serialized = BytesMut::new();

        let message = bincode::serialize(&item).map_err(Error::Serde)?;
        let sig = this.local_sk.sign(&message);

        match bincode::serialize(&SignedWireMessage { message, sig }) {
            Ok(s) => serialized.extend_from_slice(&s),
            Err(err) => return Err(Error::Io(io::Error::new(io::ErrorKind::Other, err))),
        }
        let this = self.get_mut();
        Pin::new(&mut this.framed).start_send(serialized.freeze()).map_err(Error::from)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        Pin::new(&mut this.framed).poll_flush(cx).map_err(Error::from)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        Pin::new(&mut this.framed).poll_close(cx).map_err(Error::from)
    }
}
 */
/// A message between internal threads/tasks.
#[derive(Clone, Debug)]
pub enum InternalMessageKind<C: Contribution, N: NodeId> {
    Wire(WireMessage<C, N>),
    HbMessage(Message<N>),
    HbContribution(C),
    HbChange(Change<N>),
    PeerDisconnect,
    NewIncomingConnection(InAddr, PublicKey, bool),
    NewOutgoingConnection,
    NewKeyGenInstance(mpsc::UnboundedSender<key_gen::Message>),
}

/// A message between internal threads/tasks.
#[derive(Clone, Debug)]
pub struct InternalMessage<C: Contribution, N: NodeId> {
    src_uid: Option<N>,
    src_addr: OutAddr,
    kind: InternalMessageKind<C, N>,
}

impl<C: Contribution, N: NodeId> InternalMessage<C, N> {
    pub fn new(
        src_uid: Option<N>,
        src_addr: OutAddr,
        kind: InternalMessageKind<C, N>,
    ) -> InternalMessage<C, N> {
        InternalMessage {
            src_uid,
            src_addr,
            kind,
        }
    }

    /// Returns a new `InternalMessage` without a uid.
    pub fn new_without_uid(
        src_addr: OutAddr,
        kind: InternalMessageKind<C, N>,
    ) -> InternalMessage<C, N> {
        InternalMessage::new(None, src_addr, kind)
    }

    pub fn wire(
        src_uid: Option<N>,
        src_addr: OutAddr,
        wire_message: WireMessage<C, N>,
    ) -> InternalMessage<C, N> {
        InternalMessage::new(src_uid, src_addr, InternalMessageKind::Wire(wire_message))
    }

    pub fn hb_message(src_uid: N, src_addr: OutAddr, msg: Message<N>) -> InternalMessage<C, N> {
        InternalMessage::new(Some(src_uid), src_addr, InternalMessageKind::HbMessage(msg))
    }

    pub fn hb_contribution(src_uid: N, src_addr: OutAddr, contrib: C) -> InternalMessage<C, N> {
        InternalMessage::new(
            Some(src_uid),
            src_addr,
            InternalMessageKind::HbContribution(contrib),
        )
    }

    pub fn hb_vote(src_uid: N, src_addr: OutAddr, change: Change<N>) -> InternalMessage<C, N> {
        InternalMessage::new(
            Some(src_uid),
            src_addr,
            InternalMessageKind::HbChange(change),
        )
    }

    pub fn peer_disconnect(src_uid: N, src_addr: OutAddr) -> InternalMessage<C, N> {
        InternalMessage::new(Some(src_uid), src_addr, InternalMessageKind::PeerDisconnect)
    }

    pub fn new_incoming_connection(
        src_uid: N,
        src_addr: OutAddr,
        src_in_addr: InAddr,
        src_pk: PublicKey,
        request_change_add: bool,
    ) -> InternalMessage<C, N> {
        InternalMessage::new(
            Some(src_uid),
            src_addr,
            InternalMessageKind::NewIncomingConnection(src_in_addr, src_pk, request_change_add),
        )
    }

    pub fn new_key_gen_instance(
        src_uid: N,
        src_addr: OutAddr,
        tx: mpsc::UnboundedSender<key_gen::Message>,
    ) -> InternalMessage<C, N> {
        InternalMessage::new(
            Some(src_uid),
            src_addr,
            InternalMessageKind::NewKeyGenInstance(tx),
        )
    }

    pub fn new_outgoing_connection(src_addr: OutAddr) -> InternalMessage<C, N> {
        InternalMessage::new_without_uid(src_addr, InternalMessageKind::NewOutgoingConnection)
    }

    /// Returns the source unique identifier this message was received in.
    pub fn src_uid(&self) -> Option<&N> {
        self.src_uid.as_ref()
    }

    /// Returns the source socket this message was received on.
    pub fn src_addr(&self) -> &OutAddr {
        &self.src_addr
    }

    /// Returns the internal message kind.
    pub fn kind(&self) -> &InternalMessageKind<C, N> {
        &self.kind
    }

    /// Consumes this `InternalMessage` into its parts.
    pub fn into_parts(self) -> (Option<N>, OutAddr, InternalMessageKind<C, N>) {
        (self.src_uid, self.src_addr, self.kind)
    }
}
