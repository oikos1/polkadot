// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! The Network Bridge Subsystem - protocol multiplexer for Polkadot.

use parity_scale_codec::{Encode, Decode};
use futures::prelude::*;
use futures::future::BoxFuture;
use futures::stream::BoxStream;

use sc_network::{
	ObservedRole, ReputationChange, PeerId,
	Event as NetworkEvent,
};
use sp_runtime::ConsensusEngineId;

use polkadot_subsystem::{
	FromOverseer, OverseerSignal, Subsystem, SubsystemContext, SpawnedSubsystem, SubsystemError,
	SubsystemResult,
};
use polkadot_subsystem::messages::{NetworkBridgeEvent, NetworkBridgeMessage, AllMessages};
use node_primitives::{ProtocolId, View};
use polkadot_primitives::{Block, Hash};

use std::collections::hash_map::{HashMap, Entry};
use std::pin::Pin;
use std::sync::Arc;

const MAX_VIEW_HEADS: usize = 5;

/// The engine ID of the polkadot network protocol.
pub const POLKADOT_ENGINE_ID: ConsensusEngineId = *b"dot2";
/// The protocol name.
pub const POLKADOT_PROTOCOL_NAME: &[u8] = b"/polkadot/2";

const MALFORMED_MESSAGE_COST: ReputationChange
	= ReputationChange::new(-500, "Malformed Network-bridge message");
const UNKNOWN_PROTO_COST: ReputationChange
	= ReputationChange::new(-50, "Message sent to unknown protocol");
const MALFORMED_VIEW_COST: ReputationChange
	= ReputationChange::new(-500, "Malformed view");

/// Messages received on the network.
#[derive(Encode, Decode)]
pub enum WireMessage {
	/// A message from a peer on a specific protocol.
	#[codec(index = "1")]
	ProtocolMessage(ProtocolId, Vec<u8>),
	/// A view update from a peer.
	#[codec(index = "2")]
	ViewUpdate(View),
}

/// Information about the notifications protocol. Should be used during network configuration
/// or shortly after startup to register the protocol with the network service.
pub fn notifications_protocol_info() -> (ConsensusEngineId, std::borrow::Cow<'static, [u8]>) {
	(POLKADOT_ENGINE_ID, POLKADOT_PROTOCOL_NAME.into())
}

/// An action to be carried out by the network.
#[derive(PartialEq)]
pub enum NetworkAction {
	/// Note a change in reputation for a peer.
	ReputationChange(PeerId, ReputationChange),
	/// Write a notification to a given peer.
	WriteNotification(PeerId, Vec<u8>),
}

/// An abstraction over networking for the purposes of this subsystem.
pub trait Network: Clone + Send + 'static {
	/// Get a stream of all events occurring on the network. This may include events unrelated
	/// to the Polkadot protocol - the user of this function should filter only for events related
	/// to the [`POLKADOT_ENGINE_ID`](POLKADOT_ENGINE_ID).
	fn event_stream(&mut self) -> BoxStream<'static, NetworkEvent>;

	/// Get access to an underlying sink for all network actions.
	fn action_sink<'a>(&'a mut self) -> Pin<
		Box<dyn Sink<NetworkAction, Error = SubsystemError> + Send + 'a>
	>;

	/// Report a given peer as either beneficial (+) or costly (-) according to the given scalar.
	fn report_peer(&mut self, who: PeerId, cost_benefit: ReputationChange)
		-> BoxFuture<SubsystemResult<()>>
	{
		async move {
			self.action_sink().send(NetworkAction::ReputationChange(who, cost_benefit)).await
		}.boxed()
	}

	/// Write a notification to a peer on the [`POLKADOT_ENGINE_ID`](POLKADOT_ENGINE_ID) topic.
	fn write_notification(&mut self, who: PeerId, message: Vec<u8>)
		-> BoxFuture<SubsystemResult<()>>
	{
		async move {
			self.action_sink().send(NetworkAction::WriteNotification(who, message)).await
		}.boxed()
	}
}

impl Network for Arc<sc_network::NetworkService<Block, Hash>> {
	fn event_stream(&mut self) -> BoxStream<'static, NetworkEvent> {
		sc_network::NetworkService::event_stream(self, "polkadot-network-bridge").boxed()
	}

	fn action_sink<'a>(&'a mut self)
		-> Pin<Box<dyn Sink<NetworkAction, Error = SubsystemError> + Send + 'a>>
	{
		use futures::task::{Poll, Context};

		// wrapper around a NetworkService to make it act like a sink.
		struct ActionSink<'b>(&'b sc_network::NetworkService<Block, Hash>);

		impl<'b> Sink<NetworkAction> for ActionSink<'b> {
			type Error = SubsystemError;

			fn poll_ready(self: Pin<&mut Self>, _: &mut Context) -> Poll<SubsystemResult<()>> {
				Poll::Ready(Ok(()))
			}

			fn start_send(self: Pin<&mut Self>, action: NetworkAction) -> SubsystemResult<()> {
				match action {
					NetworkAction::ReputationChange(peer, cost_benefit) => self.0.report_peer(
						peer,
						cost_benefit,
					),
					NetworkAction::WriteNotification(peer, message) => self.0.write_notification(
						peer,
						POLKADOT_ENGINE_ID,
						message,
					),
				}

				Ok(())
			}

			fn poll_flush(self: Pin<&mut Self>, _: &mut Context) -> Poll<SubsystemResult<()>> {
				Poll::Ready(Ok(()))
			}

			fn poll_close(self: Pin<&mut Self>, _: &mut Context) -> Poll<SubsystemResult<()>> {
				Poll::Ready(Ok(()))
			}
		}

		Box::pin(ActionSink(&**self))
	}
}

/// The network bridge subsystem.
pub struct NetworkBridge<N>(N);

impl<N> NetworkBridge<N> {
	/// Create a new network bridge subsystem with underlying network service.
	///
	/// This assumes that the network service has had the notifications protocol for the network
	/// bridge already registered. See [`notifications_protocol_info`](notifications_protocol_info).
	pub fn new(net_service: N) -> Self {
		NetworkBridge(net_service)
	}
}

impl<N: Network, C> Subsystem<C> for NetworkBridge<N>
	where C: SubsystemContext<Message=NetworkBridgeMessage>
{
	fn start(&mut self, ctx: C) -> SpawnedSubsystem {
		// Swallow error because failure is fatal to the node and we log with more precision
		// within `run_network`.
		SpawnedSubsystem(run_network(self.0.clone(), ctx).map(|_| ()).boxed())
	}
}

struct PeerData {
	/// Latest view sent by the peer.
	view: View,
}

enum Action {
	RegisterEventProducer(ProtocolId, fn(NetworkBridgeEvent) -> AllMessages),
	SendMessage(Vec<PeerId>, ProtocolId, Vec<u8>),
	ReportPeer(PeerId, ReputationChange),
	StartWork(Hash),
	StopWork(Hash),

	PeerConnected(PeerId, ObservedRole),
	PeerDisconnected(PeerId),
	PeerMessages(PeerId, Vec<WireMessage>),

	Abort,
}

fn action_from_overseer_message(
	res: polkadot_subsystem::SubsystemResult<FromOverseer<NetworkBridgeMessage>>,
) -> Action {
	match res {
		Ok(FromOverseer::Signal(OverseerSignal::StartWork(relay_parent)))
			=> Action::StartWork(relay_parent),
		Ok(FromOverseer::Signal(OverseerSignal::StopWork(relay_parent)))
			=> Action::StopWork(relay_parent),
		Ok(FromOverseer::Signal(OverseerSignal::Conclude)) => Action::Abort,
		Ok(FromOverseer::Communication { msg }) => match msg {
			NetworkBridgeMessage::RegisterEventProducer(protocol_id, message_producer)
				=>  Action::RegisterEventProducer(protocol_id, message_producer),
			NetworkBridgeMessage::ReportPeer(peer, rep) => Action::ReportPeer(peer, rep),
			NetworkBridgeMessage::SendMessage(peers, protocol, message)
				=> Action::SendMessage(peers, protocol, message),
		},
		Err(e) => {
			log::warn!("Shutting down Network Bridge due to error {:?}", e);
			Action::Abort
		}
	}
}

fn action_from_network_message(event: Option<NetworkEvent>) -> Option<Action> {
	match event {
		None => {
			log::warn!("Shutting down Network Bridge: underlying event stream concluded");
			Some(Action::Abort)
		}
		Some(NetworkEvent::Dht(_)) => None,
		Some(NetworkEvent::NotificationStreamOpened { remote, engine_id, role }) => {
			if engine_id == POLKADOT_ENGINE_ID {
				Some(Action::PeerConnected(remote, role))
			} else {
				None
			}
		}
		Some(NetworkEvent::NotificationStreamClosed { remote, engine_id }) => {
			if engine_id == POLKADOT_ENGINE_ID {
				Some(Action::PeerDisconnected(remote))
			} else {
				None
			}
		}
		Some(NetworkEvent::NotificationsReceived { remote, messages }) => {
			let v: Result<Vec<_>, _> = messages.iter()
				.filter(|(engine_id, _)| engine_id == &POLKADOT_ENGINE_ID)
				.map(|(_, msg_bytes)| WireMessage::decode(&mut msg_bytes.as_ref()))
				.collect();

			match v {
				Err(_) => Some(Action::ReportPeer(remote, MALFORMED_MESSAGE_COST)),
				Ok(v) => if v.is_empty() {
					None
				} else {
					Some(Action::PeerMessages(remote, v))
				}
			}
		}
	}
}

fn construct_view(live_heads: &[Hash]) -> View {
	View(live_heads.iter().rev().take(MAX_VIEW_HEADS).cloned().collect())
}

async fn dispatch_update_to_all(
	update: NetworkBridgeEvent,
	event_producers: impl IntoIterator<Item=&fn(NetworkBridgeEvent) -> AllMessages>,
	ctx: &mut impl SubsystemContext<Message=NetworkBridgeMessage>,
) -> polkadot_subsystem::SubsystemResult<()> {
	// collect messages here to avoid the borrow lasting across await boundary.
	let messages: Vec<_> = event_producers.into_iter()
		.map(|producer| producer(update.clone()))
		.collect();

	ctx.send_messages(messages).await
}

async fn update_view(
	peers: &HashMap<PeerId, PeerData>,
	live_heads: &[Hash],
	net: &mut impl Network,
	local_view: &mut View,
) -> SubsystemResult<Option<NetworkBridgeEvent>> {
	let new_view = construct_view(live_heads);
	if *local_view == new_view { return Ok(None) }
	*local_view = new_view.clone();

	let message = WireMessage::ViewUpdate(new_view.clone()).encode();

	let notifications = peers.keys().cloned()
		.map(move |peer| Ok(NetworkAction::WriteNotification(peer, message.clone())));

	net.action_sink().send_all(&mut stream::iter(notifications)).await?;

	Ok(Some(NetworkBridgeEvent::OurViewChange(local_view.clone())))
}

async fn run_network<N: Network>(
	mut net: N,
	mut ctx: impl SubsystemContext<Message=NetworkBridgeMessage>,
) -> SubsystemResult<()> {
	let mut event_stream = net.event_stream().fuse();

	// Most recent heads are at the back.
	let mut live_heads = Vec::with_capacity(MAX_VIEW_HEADS);
	let mut local_view = View(Vec::new());

	let mut peers = HashMap::new();
	let mut event_producers = HashMap::new();

	loop {
		let action = {
			let subsystem_next = ctx.recv().fuse();
			let mut net_event_next = event_stream.next().fuse();
			futures::pin_mut!(subsystem_next);

			let action = futures::select! {
				subsystem_msg = subsystem_next => Some(action_from_overseer_message(subsystem_msg)),
				net_event = net_event_next => action_from_network_message(net_event),
			};

			match action {
				Some(a) => a,
				None => continue,
			}
		};

		match action {
			Action::RegisterEventProducer(protocol_id, event_producer) => {
				// insert only if none present.
				event_producers.entry(protocol_id).or_insert(event_producer);
			}
			Action::SendMessage(peers, protocol, message) => {
				let mut message_producer = stream::iter({
					let n_peers = peers.len();
					let mut message = Some(
						WireMessage::ProtocolMessage(protocol, message).encode()
					);

					peers.iter().cloned().enumerate().map(move |(i, peer)| {
						// optimization: avoid cloning the message for the last peer in the
						// list. The message payload can be quite large. If the underlying
						// network used `Bytes` this would not be necessary.
						let message =if i == n_peers - 1 {
							message.take()
								.expect("Only taken in last iteration of loop, never afterwards; qed")
						} else {
							message.as_ref()
								.expect("Only taken in last iteratio of loop, we are not there yet; qed")
								.clone()
						};

						Ok(NetworkAction::WriteNotification(peer, message))
					})
				});

				net.action_sink().send_all(&mut message_producer).await?;
			}
			Action::ReportPeer(peer, rep) => {
				// let binding required here to avoid unnecessary sync bound.
				let report_fut = net.report_peer(peer, rep);
				report_fut.await?
			}
			Action::StartWork(relay_parent) => {
				println!("New relay parent. Updating view.");
				live_heads.push(relay_parent);
				if let Some(view_update)
					= update_view(&peers, &live_heads, &mut net, &mut local_view).await?
				{
					if let Err(e) = dispatch_update_to_all(
						view_update,
						event_producers.values(),
						&mut ctx,
					).await {
						log::warn!("Aborting - Failure to dispatch messages to overseer");
						return Err(e)
					}
				}
			}
			Action::StopWork(relay_parent) => {
				live_heads.retain(|h| h != &relay_parent);
				if let Some(view_update)
					= update_view(&peers, &live_heads, &mut net, &mut local_view).await?
				{
					if let Err(e) = dispatch_update_to_all(
						view_update,
						event_producers.values(),
						&mut ctx,
					).await {
						log::warn!("Aborting - Failure to dispatch messages to overseer");
						return Err(e)
					}
				}
			}

			Action::PeerConnected(peer, role) => {
				println!("New peer {:?}", peer);
				match peers.entry(peer.clone()) {
					Entry::Occupied(_) => continue,
					Entry::Vacant(vacant) => {
						vacant.insert(PeerData {
							view: View(Vec::new()),
						});

						if let Err(e) = dispatch_update_to_all(
							NetworkBridgeEvent::PeerConnected(peer, role),
							event_producers.values(),
							&mut ctx,
						).await {
							log::warn!("Aborting - Failure to dispatch messages to overseer");
							return Err(e)
						}
					}
				}
			}
			Action::PeerDisconnected(peer) => {
				if peers.remove(&peer).is_some() {
					if let Err(e) = dispatch_update_to_all(
						NetworkBridgeEvent::PeerDisconnected(peer),
						event_producers.values(),
						&mut ctx,
					).await {
						log::warn!("Aborting - Failure to dispatch messages to overseer");
						return Err(e)
					}
				}
			},
			Action::PeerMessages(peer, messages) => {
				let peer_data = match peers.get_mut(&peer) {
					None => continue,
					Some(d) => d,
				};

				let mut outgoing_messages = Vec::with_capacity(messages.len());
				for message in messages {
					match message {
						WireMessage::ViewUpdate(new_view) => {
							if new_view.0.len() > MAX_VIEW_HEADS {
								let report_fut = net.report_peer(
									peer.clone(),
									MALFORMED_VIEW_COST,
								);

								report_fut.await?;
								continue
							}

							if new_view == peer_data.view { continue }
							peer_data.view = new_view;

							let update = NetworkBridgeEvent::PeerViewChange(
								peer.clone(),
								peer_data.view.clone(),
							);

							outgoing_messages.extend(
								event_producers.values().map(|producer| producer(update.clone()))
							);
						}
						WireMessage::ProtocolMessage(protocol, message) => {
							let message = match event_producers.get(&protocol) {
								Some(producer) => Some(producer(
									NetworkBridgeEvent::PeerMessage(peer.clone(), message)
								)),
								None => {
									let report_fut = net.report_peer(
										peer.clone(),
										UNKNOWN_PROTO_COST,
									);

									report_fut.await?;
									None
								}
							};

							if let Some(message) = message {
								outgoing_messages.push(message);
							}
						}
					}
				}

				let send_messages = ctx.send_messages(outgoing_messages);
				if let Err(e) = send_messages.await {
					log::warn!("Aborting - Failure to dispatch messages to overseer");
					return Err(e)
				}
			},

			Action::Abort => return Ok(()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures::channel::mpsc;
	use futures::executor::{self, ThreadPool};
	use std::sync::Arc;
	use parking_lot::Mutex;

	// The subsystem's view of the network - only supports a single call to `event_stream`.
	#[derive(Clone)]
	struct TestNetwork {
		net_events: Arc<Mutex<Option<mpsc::UnboundedReceiver<NetworkEvent>>>>,
		action_tx: mpsc::UnboundedSender<NetworkAction>,
	}

	// The test's view of the network. This receives updates from the subsystem in the form
	// of `NetworkAction`s.
	struct TestNetworkHandle {
		action_rx: mpsc::UnboundedReceiver<NetworkAction>,
		net_tx: mpsc::UnboundedSender<NetworkEvent>,
	}

	fn new_test_network() -> (
		TestNetwork,
		TestNetworkHandle,
	) {
		let (net_tx, net_rx) = mpsc::unbounded();
		let (action_tx, action_rx) = mpsc::unbounded();

		(
			TestNetwork {
				net_events: Arc::new(Mutex::new(Some(net_rx))),
				action_tx,
			},
			TestNetworkHandle {
				action_rx,
				net_tx,
			},
		)
	}

	impl Network for TestNetwork {
		fn event_stream(&mut self) -> BoxStream<NetworkEvent> {
			self.net_events.lock()
				.take()
				.expect("Subsystem made more than one call to `event_stream`")
				.boxed()
		}

		fn report_peer(&mut self, who: PeerId, cost_benefit: ReputationChange) -> BoxFuture<()> {
			let x = async move {
				self.action_tx.unbounded_send(NetworkAction::ReputationChange(who, cost_benefit))
					.expect("test hung up on network");
			};

			x.boxed()
		}

		fn write_notification(&mut self, who: PeerId, message: Vec<u8>) -> BoxFuture<()> {
			let x = async move {
				self.action_tx.unbounded_send(NetworkAction::WriteNotification(who, message))
					.expect("test hung up on network");
			};

			x.boxed()
		}
	}

	impl TestNetworkHandle {
		// Get the next network action.
		async fn next_network_action(&mut self) -> NetworkAction {
			self.action_rx.next().await.expect("subsystem concluded early")
		}

		// Wait for the next N network actions.
		async fn next_network_actions(&mut self, n: usize) -> Vec<NetworkAction> {
			let mut v = Vec::with_capacity(n);
			for _ in 0..n {
				v.push(self.next_network_action().await);
			}

			v
		}

		fn connect_peer(&self, peer: PeerId, role: ObservedRole) {
			self.send_network_event(NetworkEvent::NotificationStreamOpened {
				remote: peer,
				engine_id: POLKADOT_ENGINE_ID,
				role,
			});
		}

		fn disconnect_peer(&self, peer: PeerId) {
			self.send_network_event(NetworkEvent::NotificationStreamClosed {
				remote: peer,
				engine_id: POLKADOT_ENGINE_ID,
			});
		}

		fn peer_message(&self, peer: PeerId, message: Vec<u8>) {
			self.send_network_event(NetworkEvent::NotificationsReceived {
				remote: peer,
				messages: vec![(POLKADOT_ENGINE_ID, message.into())],
			});
		}

		fn send_network_event(&self, event: NetworkEvent) {
			self.net_tx.unbounded_send(event).expect("subsystem concluded early");
		}
	}

	fn network_actions_contains(actions: &[NetworkAction], action: &NetworkAction) -> bool {
		actions.iter().find(|&x| x == action).is_some()
	}

	#[test]
	fn sends_view_updates_to_peers() {
		let pool = ThreadPool::new().unwrap();

		let (network, mut network_handle) = new_test_network();
		let (context, virtual_overseer) = subsystem_test::make_subsystem_context(pool.clone());
		pool.spawn_ok(run_network(network, context));

		executor::block_on(async move {
			let peer_a = PeerId::random();
			let peer_b = PeerId::random();

			network_handle.connect_peer(peer_a.clone(), ObservedRole::Full);
			network_handle.connect_peer(peer_b.clone(), ObservedRole::Full);

			let hash_a = Hash::from([1; 32]);

			virtual_overseer.send(FromOverseer::Signal(OverseerSignal::StartWork(hash_a)));

			let actions = network_handle.next_network_actions(2).await;
			let wire_message = WireMessage::ViewUpdate(View(vec![hash_a])).encode();
			assert!(network_actions_contains(
				&actions,
				&NetworkAction::WriteNotification(peer_a, wire_message.clone()),
			));

			assert!(network_actions_contains(
				&actions,
				&NetworkAction::WriteNotification(peer_b, wire_message.clone()),
			));
		});
	}


	// TODO [now]: our view updates are sent.
	// TODO [now]: peer view updates get sent via overseer.

	// TODO [now]: peer messages are sent via event producer
}
