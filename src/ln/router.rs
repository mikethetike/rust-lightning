use secp256k1::key::PublicKey;
use secp256k1::{Secp256k1,Message};

use bitcoin::util::hash::Sha256dHash;

use ln::msgs::{ErrorAction,HandleError,RoutingMessageHandler,MsgEncodable,NetAddress,GlobalFeatures};
use ln::msgs;

use std::cmp;
use std::sync::RwLock;
use std::collections::{HashMap,BinaryHeap};
use std::collections::hash_map::Entry;

/// A hop in a route
#[derive(Clone)]
pub struct RouteHop {
	pub pubkey: PublicKey,
	/// The channel that should be used from the previous hop to reach this node.
	pub short_channel_id: u64,
	/// The fee taken on this hop. For the last hop, this should be the full value of the payment.
	pub fee_msat: u64,
	/// The CLTV delta added for this hop. For the last hop, this should be the full CLTV value
	/// expected at the destination, NOT a delta.
	pub cltv_expiry_delta: u32,
}

/// A route from us through the network to a destination
#[derive(Clone)]
pub struct Route {
	/// The list of hops, NOT INCLUDING our own, where the last hop is the destination. Thus, this
	/// must always be at least length one. By protocol rules, this may not currently exceed 20 in
	/// length.
	pub hops: Vec<RouteHop>,
}

struct DirectionalChannelInfo {
	src_node_id: PublicKey,
	last_update: u32,
	enabled: bool,
	cltv_expiry_delta: u16,
	htlc_minimum_msat: u64,
	fee_base_msat: u32,
	fee_proportional_millionths: u32,
}

struct ChannelInfo {
	features: GlobalFeatures,
	one_to_two: DirectionalChannelInfo,
	two_to_one: DirectionalChannelInfo,
}

struct NodeInfo {
	#[cfg(feature = "non_bitcoin_chain_hash_routing")]
	channels: Vec<(u64, Sha256dHash)>,
	#[cfg(not(feature = "non_bitcoin_chain_hash_routing"))]
	channels: Vec<u64>,

	lowest_inbound_channel_fee_base_msat: u32,
	lowest_inbound_channel_fee_proportional_millionths: u32,

	features: GlobalFeatures,
	last_update: u32,
	rgb: [u8; 3],
	alias: [u8; 32],
	addresses: Vec<NetAddress>,
}

struct NetworkMap {
	#[cfg(feature = "non_bitcoin_chain_hash_routing")]
	channels: HashMap<(u64, Sha256dHash), ChannelInfo>,
	#[cfg(not(feature = "non_bitcoin_chain_hash_routing"))]
	channels: HashMap<u64, ChannelInfo>,

	our_node_id: PublicKey,
	nodes: HashMap<PublicKey, NodeInfo>,
}

impl NetworkMap {
	#[cfg(feature = "non_bitcoin_chain_hash_routing")]
	#[inline]
	fn get_key(short_channel_id: u64, chain_hash: Sha256dHash) -> (u64, Sha256dHash) {
		(short_channel_id, chain_hash)
	}

	#[cfg(not(feature = "non_bitcoin_chain_hash_routing"))]
	#[inline]
	fn get_key(short_channel_id: u64, _: Sha256dHash) -> u64 {
		short_channel_id
	}
}

/// A channel descriptor which provides a last-hop route to get_route
pub struct RouteHint {
	pub src_node_id: PublicKey,
	pub short_channel_id: u64,
	pub fee_base_msat: u64,
	pub fee_proportional_millionths: u32,
	pub cltv_expiry_delta: u16,
	pub htlc_minimum_msat: u64,
}

/// Tracks a view of the network, receiving updates from peers and generating Routes to
/// payment destinations.
pub struct Router {
	secp_ctx: Secp256k1,
	network_map: RwLock<NetworkMap>,
}

macro_rules! secp_verify_sig {
	( $secp_ctx: expr, $msg: expr, $sig: expr, $pubkey: expr ) => {
		match $secp_ctx.verify($msg, $sig, $pubkey) {
			Ok(_) => {},
			Err(_) => return Err(HandleError{err: "Invalid signature from remote node", msg: None}),
		}
	};
}

impl RoutingMessageHandler for Router {
	fn handle_node_announcement(&self, msg: &msgs::NodeAnnouncement) -> Result<(), HandleError> {
		let msg_hash = Message::from_slice(&Sha256dHash::from_data(&msg.contents.encode()[..])[..]).unwrap();
		secp_verify_sig!(self.secp_ctx, &msg_hash, &msg.signature, &msg.contents.node_id);

		let mut network = self.network_map.write().unwrap();
		match network.nodes.get_mut(&msg.contents.node_id) {
			None => Err(HandleError{err: "No existing channels for node_announcement", msg: Some(ErrorAction::IgnoreError)}),
			Some(node) => {
				if node.last_update >= msg.contents.timestamp {
					return Err(HandleError{err: "Update older than last processed update", msg: Some(ErrorAction::IgnoreError)});
				}

				node.features = msg.contents.features.clone();
				node.last_update = msg.contents.timestamp;
				node.rgb = msg.contents.rgb;
				node.alias = msg.contents.alias;
				node.addresses = msg.contents.addresses.clone();
				Ok(())
			}
		}
	}

	fn handle_channel_announcement(&self, msg: &msgs::ChannelAnnouncement) -> Result<bool, HandleError> {
		let msg_hash = Message::from_slice(&Sha256dHash::from_data(&msg.contents.encode()[..])[..]).unwrap();
		secp_verify_sig!(self.secp_ctx, &msg_hash, &msg.node_signature_1, &msg.contents.node_id_1);
		secp_verify_sig!(self.secp_ctx, &msg_hash, &msg.node_signature_2, &msg.contents.node_id_2);
		secp_verify_sig!(self.secp_ctx, &msg_hash, &msg.bitcoin_signature_1, &msg.contents.bitcoin_key_1);
		secp_verify_sig!(self.secp_ctx, &msg_hash, &msg.bitcoin_signature_2, &msg.contents.bitcoin_key_2);

		//TODO: Call blockchain thing to ask if the short_channel_id is valid
		//TODO: Only allow bitcoin chain_hash

		if msg.contents.features.requires_unknown_bits() {
			return Err(HandleError{err: "Channel announcement required unknown feature flags", msg: None});
		}

		let mut network = self.network_map.write().unwrap();

		match network.channels.entry(NetworkMap::get_key(msg.contents.short_channel_id, msg.contents.chain_hash)) {
			Entry::Occupied(_) => {
				//TODO: because asking the blockchain if short_channel_id is valid is only optional
				//in the blockchain API, we need to handle it smartly here, though its unclear
				//exactly how...
				return Err(HandleError{err: "Already have knowledge of channel", msg: Some(ErrorAction::IgnoreError)})
			},
			Entry::Vacant(entry) => {
				entry.insert(ChannelInfo {
					features: msg.contents.features.clone(),
					one_to_two: DirectionalChannelInfo {
						src_node_id: msg.contents.node_id_1.clone(),
						last_update: 0,
						enabled: false,
						cltv_expiry_delta: u16::max_value(),
						htlc_minimum_msat: u64::max_value(),
						fee_base_msat: u32::max_value(),
						fee_proportional_millionths: u32::max_value(),
					},
					two_to_one: DirectionalChannelInfo {
						src_node_id: msg.contents.node_id_2.clone(),
						last_update: 0,
						enabled: false,
						cltv_expiry_delta: u16::max_value(),
						htlc_minimum_msat: u64::max_value(),
						fee_base_msat: u32::max_value(),
						fee_proportional_millionths: u32::max_value(),
					}
				});
			}
		};

		macro_rules! add_channel_to_node {
			( $node_id: expr ) => {
				match network.nodes.entry($node_id) {
					Entry::Occupied(node_entry) => {
						node_entry.into_mut().channels.push(NetworkMap::get_key(msg.contents.short_channel_id, msg.contents.chain_hash));
					},
					Entry::Vacant(node_entry) => {
						node_entry.insert(NodeInfo {
							channels: vec!(NetworkMap::get_key(msg.contents.short_channel_id, msg.contents.chain_hash)),
							lowest_inbound_channel_fee_base_msat: u32::max_value(),
							lowest_inbound_channel_fee_proportional_millionths: u32::max_value(),
							features: GlobalFeatures::new(),
							last_update: 0,
							rgb: [0; 3],
							alias: [0; 32],
							addresses: Vec::new(),
						});
					}
				}
			};
		}

		add_channel_to_node!(msg.contents.node_id_1);
		add_channel_to_node!(msg.contents.node_id_2);

		Ok(!msg.contents.features.supports_unknown_bits())
	}

	fn handle_htlc_fail_channel_update(&self, update: &msgs::HTLCFailChannelUpdate) {
		match update {
			&msgs::HTLCFailChannelUpdate::ChannelUpdateMessage { ref msg } => {
				let _ = self.handle_channel_update(msg);
			},
			&msgs::HTLCFailChannelUpdate::ChannelClosed { ref short_channel_id } => {
				let mut network = self.network_map.write().unwrap();
				network.channels.remove(short_channel_id);
			},
		}
	}

	fn handle_channel_update(&self, msg: &msgs::ChannelUpdate) -> Result<(), HandleError> {
		let mut network = self.network_map.write().unwrap();
		let dest_node_id;
		let chan_enabled = msg.contents.flags & (1 << 1) != (1 << 1);
		let chan_was_enabled;

		match network.channels.get_mut(&NetworkMap::get_key(msg.contents.short_channel_id, msg.contents.chain_hash)) {
			None => return Err(HandleError{err: "Couldn't find channel for update", msg: Some(ErrorAction::IgnoreError)}),
			Some(channel) => {
				macro_rules! maybe_update_channel_info {
					( $target: expr) => {
						if $target.last_update >= msg.contents.timestamp {
							return Err(HandleError{err: "Update older than last processed update", msg: Some(ErrorAction::IgnoreError)});
						}
						chan_was_enabled = $target.enabled;
						$target.last_update = msg.contents.timestamp;
						$target.enabled = chan_enabled;
						$target.cltv_expiry_delta = msg.contents.cltv_expiry_delta;
						$target.htlc_minimum_msat = msg.contents.htlc_minimum_msat;
						$target.fee_base_msat = msg.contents.fee_base_msat;
						$target.fee_proportional_millionths = msg.contents.fee_proportional_millionths;
					}
				}

				let msg_hash = Message::from_slice(&Sha256dHash::from_data(&msg.contents.encode()[..])[..]).unwrap();
				if msg.contents.flags & 1 == 1 {
					dest_node_id = channel.one_to_two.src_node_id.clone();
					secp_verify_sig!(self.secp_ctx, &msg_hash, &msg.signature, &channel.two_to_one.src_node_id);
					maybe_update_channel_info!(channel.two_to_one);
				} else {
					dest_node_id = channel.two_to_one.src_node_id.clone();
					secp_verify_sig!(self.secp_ctx, &msg_hash, &msg.signature, &channel.one_to_two.src_node_id);
					maybe_update_channel_info!(channel.one_to_two);
				}
			}
		}

		if chan_enabled {
			let node = network.nodes.get_mut(&dest_node_id).unwrap();
			node.lowest_inbound_channel_fee_base_msat = cmp::min(node.lowest_inbound_channel_fee_base_msat, msg.contents.fee_base_msat);
			node.lowest_inbound_channel_fee_proportional_millionths = cmp::min(node.lowest_inbound_channel_fee_proportional_millionths, msg.contents.fee_proportional_millionths);
		} else if chan_was_enabled {
			let mut lowest_inbound_channel_fee_base_msat = u32::max_value();
			let mut lowest_inbound_channel_fee_proportional_millionths = u32::max_value();

			{
				let node = network.nodes.get(&dest_node_id).unwrap();

				for chan_id in node.channels.iter() {
					let chan = network.channels.get(chan_id).unwrap();
					if chan.one_to_two.src_node_id == dest_node_id {
						lowest_inbound_channel_fee_base_msat = cmp::min(lowest_inbound_channel_fee_base_msat, chan.two_to_one.fee_base_msat);
						lowest_inbound_channel_fee_proportional_millionths = cmp::min(lowest_inbound_channel_fee_proportional_millionths, chan.two_to_one.fee_proportional_millionths);
					} else {
						lowest_inbound_channel_fee_base_msat = cmp::min(lowest_inbound_channel_fee_base_msat, chan.one_to_two.fee_base_msat);
						lowest_inbound_channel_fee_proportional_millionths = cmp::min(lowest_inbound_channel_fee_proportional_millionths, chan.one_to_two.fee_proportional_millionths);
					}
				}
			}

			//TODO: satisfy the borrow-checker without a double-map-lookup :(
			let mut_node = network.nodes.get_mut(&dest_node_id).unwrap();
			mut_node.lowest_inbound_channel_fee_base_msat = lowest_inbound_channel_fee_base_msat;
			mut_node.lowest_inbound_channel_fee_proportional_millionths = lowest_inbound_channel_fee_proportional_millionths;
		}

		Ok(())
	}
}

#[derive(Eq, PartialEq)]
struct RouteGraphNode {
	pubkey: PublicKey,
	lowest_fee_to_peer_through_node: u64,
}

impl cmp::Ord for RouteGraphNode {
	fn cmp(&self, other: &RouteGraphNode) -> cmp::Ordering {
		other.lowest_fee_to_peer_through_node.cmp(&self.lowest_fee_to_peer_through_node)
			.then_with(|| other.pubkey.serialize().cmp(&self.pubkey.serialize()))
	}
}

impl cmp::PartialOrd for RouteGraphNode {
	fn partial_cmp(&self, other: &RouteGraphNode) -> Option<cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Router {
	pub fn new(our_pubkey: PublicKey) -> Router {
		let mut nodes = HashMap::new();
		nodes.insert(our_pubkey.clone(), NodeInfo {
			channels: Vec::new(),
			lowest_inbound_channel_fee_base_msat: u32::max_value(),
			lowest_inbound_channel_fee_proportional_millionths: u32::max_value(),
			features: GlobalFeatures::new(),
			last_update: 0,
			rgb: [0; 3],
			alias: [0; 32],
			addresses: Vec::new(),
		});
		Router {
			secp_ctx: Secp256k1::new(),
			network_map: RwLock::new(NetworkMap {
				channels: HashMap::new(),
				our_node_id: our_pubkey,
				nodes: nodes,
			}),
		}
	}

	/// Marks a node as having failed a route. This will avoid re-using the node in routes for now,
	/// with an expotnential decay in node "badness". Note that there is deliberately no
	/// mark_channel_bad as a node may simply lie and suggest that an upstream channel from it is
	/// what failed the route and not the node itself. Instead, setting the blamed_upstream_node
	/// boolean will reduce the penalty, returning the node to usability faster. If the node is
	/// behaving correctly, it will disable the failing channel and we will use it again next time.
	pub fn mark_node_bad(&self, _node_id: &PublicKey, _blamed_upstream_node: bool) {
		unimplemented!();
	}

	/// Gets a route from us to the given target node.
	/// Extra routing hops between known nodes and the target will be used if they are included in
	/// last_hops.
	/// The fees on channels from us to next-hops are ignored (as they are assumed to all be
	/// equal), however the enabled/disabled bit on such channels as well as the htlc_minimum_msat
	/// *is* checked as they may change based on the receiving node.
	pub fn get_route(&self, target: &PublicKey, last_hops: &Vec<RouteHint>, final_value_msat: u64, final_cltv: u32) -> Result<Route, HandleError> {
		// TODO: Obviously *only* using total fee cost sucks. We should consider weighting by
		// uptime/success in using a node in the past.
		let network = self.network_map.read().unwrap();

		if *target == network.our_node_id {
			return Err(HandleError{err: "Cannot generate a route to ourselves", msg: None});
		}

		// We do a dest-to-source Dijkstra's sorting by each node's distance from the destination
		// plus the minimum per-HTLC fee to get from it to another node (aka "shitty A*").
		// TODO: There are a few tweaks we could do, including possibly pre-calculating more stuff
		// to use as the A* heuristic beyond just the cost to get one node further than the current
		// one.

		let mut targets = BinaryHeap::new(); //TODO: Do we care about switching to eg Fibbonaci heap?
		let mut dist = HashMap::with_capacity(network.nodes.len());
		for (key, node) in network.nodes.iter() {
			dist.insert(key.clone(), (u64::max_value(),
				node.lowest_inbound_channel_fee_base_msat as u64,
				node.lowest_inbound_channel_fee_proportional_millionths as u64,
				RouteHop {
					pubkey: PublicKey::new(),
					short_channel_id: 0,
					fee_msat: 0,
					cltv_expiry_delta: 0,
			}));
		}

		macro_rules! add_entry {
			// Adds entry which goes from the node pointed to by $directional_info to
			// $dest_node_id over the channel with id $chan_id with fees described in
			// $directional_info.
			( $chan_id: expr, $dest_node_id: expr, $directional_info: expr, $starting_fee_msat: expr ) => {
				//TODO: Explore simply adding fee to hit htlc_minimum_msat
				if $starting_fee_msat as u64 + final_value_msat > $directional_info.htlc_minimum_msat {
					let new_fee = $directional_info.fee_base_msat as u64 + ($starting_fee_msat + final_value_msat) * ($directional_info.fee_proportional_millionths as u64) / 1000000;
					let mut total_fee = $starting_fee_msat as u64;
					let old_entry = dist.get_mut(&$directional_info.src_node_id).unwrap();
					if $directional_info.src_node_id != network.our_node_id {
						// Ignore new_fee for channel-from-us as we assume all channels-from-us
						// will have the same effective-fee
						total_fee += new_fee;
						total_fee += old_entry.2 * (final_value_msat + total_fee) / 1000000 + old_entry.1;
					}
					let new_graph_node = RouteGraphNode {
						pubkey: $directional_info.src_node_id,
						lowest_fee_to_peer_through_node: total_fee,
					};
					if old_entry.0 > total_fee {
						targets.push(new_graph_node);
						old_entry.0 = total_fee;
						old_entry.3 = RouteHop {
							pubkey: $dest_node_id.clone(),
							short_channel_id: $chan_id.clone(),
							fee_msat: new_fee, // This field is ignored on the last-hop anyway
							cltv_expiry_delta: $directional_info.cltv_expiry_delta as u32,
						}
					}
				}
			};
		}

		macro_rules! add_entries_to_cheapest_to_target_node {
			( $node: expr, $node_id: expr, $fee_to_target_msat: expr ) => {
				for chan_id in $node.channels.iter() {
					let chan = network.channels.get(chan_id).unwrap();
					if chan.one_to_two.src_node_id == *$node_id {
						// ie $node is one, ie next hop in A* is two, via the two_to_one channel
						if chan.two_to_one.enabled {
							add_entry!(chan_id, chan.one_to_two.src_node_id, chan.two_to_one, $fee_to_target_msat);
						}
					} else {
						if chan.one_to_two.enabled {
							add_entry!(chan_id, chan.two_to_one.src_node_id, chan.one_to_two, $fee_to_target_msat);
						}
					}
				}
			};
		}

		match network.nodes.get(target) {
			None => {},
			Some(node) => {
				add_entries_to_cheapest_to_target_node!(node, target, 0);
			},
		}

		for hop in last_hops.iter() {
			if network.nodes.get(&hop.src_node_id).is_some() {
				add_entry!(hop.short_channel_id, target, hop, 0);
			}
		}

		while let Some(RouteGraphNode { pubkey, lowest_fee_to_peer_through_node }) = targets.pop() {
			if pubkey == network.our_node_id {
				let mut res = vec!(dist.remove(&network.our_node_id).unwrap().3);
				while res.last().unwrap().pubkey != *target {
					let new_entry = dist.remove(&res.last().unwrap().pubkey).unwrap().3;
					res.last_mut().unwrap().fee_msat = new_entry.fee_msat;
					res.last_mut().unwrap().cltv_expiry_delta = new_entry.cltv_expiry_delta;
					res.push(new_entry);
				}
				res.last_mut().unwrap().fee_msat = final_value_msat;
				res.last_mut().unwrap().cltv_expiry_delta = final_cltv;
				return Ok(Route {
					hops: res
				});
			}

			match network.nodes.get(&pubkey) {
				None => {},
				Some(node) => {
					let mut fee = lowest_fee_to_peer_through_node - node.lowest_inbound_channel_fee_base_msat as u64;
					fee -= node.lowest_inbound_channel_fee_proportional_millionths as u64 * (fee + final_value_msat) / 1000000;
					add_entries_to_cheapest_to_target_node!(node, &pubkey, fee);
				},
			}
		}

		Err(HandleError{err: "Failed to find a path to the given destination", msg: None})
	}
}

#[cfg(test)]
mod tests {
	use ln::router::{Router,NodeInfo,NetworkMap,ChannelInfo,DirectionalChannelInfo,RouteHint};
	use ln::msgs::GlobalFeatures;

	use bitcoin::util::misc::hex_bytes;
	use bitcoin::util::hash::Sha256dHash;

	use secp256k1::key::{PublicKey,SecretKey};
	use secp256k1::Secp256k1;

	#[test]
	fn route_test() {
		let secp_ctx = Secp256k1::new();
		let our_id = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&secp_ctx, &hex_bytes("0101010101010101010101010101010101010101010101010101010101010101").unwrap()[..]).unwrap()).unwrap();
		let router = Router::new(our_id);

		// Build network from our_id to node8:
		//
		//        -1(1)2- node1 -1(3)2-
		//       /                     \
		// our_id                       - node3
		//       \                     /
		//        -1(2)2- node2 -1(4)2-
		//
		//
		// chan1 1-to-2: disabled
		// chan1 2-to-1: enabled, 0 fee
		//
		// chan2 1-to-2: enabled, ignored fee
		// chan2 2-to-1: enabled, 0 fee
		//
		// chan3 1-to-2: enabled, 0 fee
		// chan3 2-to-1: enabled, 100 msat fee
		//
		// chan4 1-to-2: enabled, 100% fee
		// chan4 2-to-1: enabled, 0 fee
		//
		//
		//
		//       -1(5)2- node4 -1(8)2--
		//       |         2          |
		//       |       (11)         |
		//      /          1           \
		// node3--1(6)2- node5 -1(9)2--- node7 (not in global route map)
		//      \                      /
		//       -1(7)2- node6 -1(10)2-
		//
		// chan5  1-to-2: enabled, 100 msat fee
		// chan5  2-to-1: enabled, 0 fee
		//
		// chan6  1-to-2: enabled, 0 fee
		// chan6  2-to-1: enabled, 0 fee
		//
		// chan7  1-to-2: enabled, 100% fee
		// chan7  2-to-1: enabled, 0 fee
		//
		// chan8  1-to-2: enabled, variable fee (0 then 1000 msat)
		// chan8  2-to-1: enabled, 0 fee
		//
		// chan9  1-to-2: enabled, 1001 msat fee
		// chan9  2-to-1: enabled, 0 fee
		//
		// chan10 1-to-2: enabled, 0 fee
		// chan10 2-to-1: enabled, 0 fee
		//
		// chan11 1-to-2: enabled, 0 fee
		// chan11 2-to-1: enabled, 0 fee

		let node1 = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&secp_ctx, &hex_bytes("0202020202020202020202020202020202020202020202020202020202020202").unwrap()[..]).unwrap()).unwrap();
		let node2 = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&secp_ctx, &hex_bytes("0303030303030303030303030303030303030303030303030303030303030303").unwrap()[..]).unwrap()).unwrap();
		let node3 = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&secp_ctx, &hex_bytes("0404040404040404040404040404040404040404040404040404040404040404").unwrap()[..]).unwrap()).unwrap();
		let node4 = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&secp_ctx, &hex_bytes("0505050505050505050505050505050505050505050505050505050505050505").unwrap()[..]).unwrap()).unwrap();
		let node5 = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&secp_ctx, &hex_bytes("0606060606060606060606060606060606060606060606060606060606060606").unwrap()[..]).unwrap()).unwrap();
		let node6 = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&secp_ctx, &hex_bytes("0707070707070707070707070707070707070707070707070707070707070707").unwrap()[..]).unwrap()).unwrap();
		let node7 = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&secp_ctx, &hex_bytes("0808080808080808080808080808080808080808080808080808080808080808").unwrap()[..]).unwrap()).unwrap();

		let zero_hash = Sha256dHash::from_data(&[0; 32]);

		{
			let mut network = router.network_map.write().unwrap();

			network.nodes.insert(node1.clone(), NodeInfo {
				channels: vec!(NetworkMap::get_key(1, zero_hash.clone()), NetworkMap::get_key(3, zero_hash.clone())),
				lowest_inbound_channel_fee_base_msat: 100,
				lowest_inbound_channel_fee_proportional_millionths: 0,
				features: GlobalFeatures::new(),
				last_update: 1,
				rgb: [0; 3],
				alias: [0; 32],
				addresses: Vec::new(),
			});
			network.channels.insert(NetworkMap::get_key(1, zero_hash.clone()), ChannelInfo {
				features: GlobalFeatures::new(),
				one_to_two: DirectionalChannelInfo {
					src_node_id: our_id.clone(),
					last_update: 0,
					enabled: false,
					cltv_expiry_delta: u16::max_value(), // This value should be ignored
					htlc_minimum_msat: 0,
					fee_base_msat: u32::max_value(), // This value should be ignored
					fee_proportional_millionths: u32::max_value(), // This value should be ignored
				}, two_to_one: DirectionalChannelInfo {
					src_node_id: node1.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: 0,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				},
			});
			network.nodes.insert(node2.clone(), NodeInfo {
				channels: vec!(NetworkMap::get_key(2, zero_hash.clone()), NetworkMap::get_key(4, zero_hash.clone())),
				lowest_inbound_channel_fee_base_msat: 0,
				lowest_inbound_channel_fee_proportional_millionths: 0,
				features: GlobalFeatures::new(),
				last_update: 1,
				rgb: [0; 3],
				alias: [0; 32],
				addresses: Vec::new(),
			});
			network.channels.insert(NetworkMap::get_key(2, zero_hash.clone()), ChannelInfo {
				features: GlobalFeatures::new(),
				one_to_two: DirectionalChannelInfo {
					src_node_id: our_id.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: u16::max_value(), // This value should be ignored
					htlc_minimum_msat: 0,
					fee_base_msat: u32::max_value(), // This value should be ignored
					fee_proportional_millionths: u32::max_value(), // This value should be ignored
				}, two_to_one: DirectionalChannelInfo {
					src_node_id: node2.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: 0,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				},
			});
			network.nodes.insert(node3.clone(), NodeInfo {
				channels: vec!(
					NetworkMap::get_key(3, zero_hash.clone()),
					NetworkMap::get_key(4, zero_hash.clone()),
					NetworkMap::get_key(5, zero_hash.clone()),
					NetworkMap::get_key(6, zero_hash.clone()),
					NetworkMap::get_key(7, zero_hash.clone())),
				lowest_inbound_channel_fee_base_msat: 0,
				lowest_inbound_channel_fee_proportional_millionths: 0,
				features: GlobalFeatures::new(),
				last_update: 1,
				rgb: [0; 3],
				alias: [0; 32],
				addresses: Vec::new(),
			});
			network.channels.insert(NetworkMap::get_key(3, zero_hash.clone()), ChannelInfo {
				features: GlobalFeatures::new(),
				one_to_two: DirectionalChannelInfo {
					src_node_id: node1.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (3 << 8) | 1,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				}, two_to_one: DirectionalChannelInfo {
					src_node_id: node3.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (3 << 8) | 2,
					htlc_minimum_msat: 0,
					fee_base_msat: 100,
					fee_proportional_millionths: 0,
				},
			});
			network.channels.insert(NetworkMap::get_key(4, zero_hash.clone()), ChannelInfo {
				features: GlobalFeatures::new(),
				one_to_two: DirectionalChannelInfo {
					src_node_id: node2.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (4 << 8) | 1,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 1000000,
				}, two_to_one: DirectionalChannelInfo {
					src_node_id: node3.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (4 << 8) | 2,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				},
			});
			network.nodes.insert(node4.clone(), NodeInfo {
				channels: vec!(NetworkMap::get_key(5, zero_hash.clone()), NetworkMap::get_key(11, zero_hash.clone())),
				lowest_inbound_channel_fee_base_msat: 0,
				lowest_inbound_channel_fee_proportional_millionths: 0,
				features: GlobalFeatures::new(),
				last_update: 1,
				rgb: [0; 3],
				alias: [0; 32],
				addresses: Vec::new(),
			});
			network.channels.insert(NetworkMap::get_key(5, zero_hash.clone()), ChannelInfo {
				features: GlobalFeatures::new(),
				one_to_two: DirectionalChannelInfo {
					src_node_id: node3.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (5 << 8) | 1,
					htlc_minimum_msat: 0,
					fee_base_msat: 100,
					fee_proportional_millionths: 0,
				}, two_to_one: DirectionalChannelInfo {
					src_node_id: node4.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (5 << 8) | 2,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				},
			});
			network.nodes.insert(node5.clone(), NodeInfo {
				channels: vec!(NetworkMap::get_key(6, zero_hash.clone()), NetworkMap::get_key(11, zero_hash.clone())),
				lowest_inbound_channel_fee_base_msat: 0,
				lowest_inbound_channel_fee_proportional_millionths: 0,
				features: GlobalFeatures::new(),
				last_update: 1,
				rgb: [0; 3],
				alias: [0; 32],
				addresses: Vec::new(),
			});
			network.channels.insert(NetworkMap::get_key(6, zero_hash.clone()), ChannelInfo {
				features: GlobalFeatures::new(),
				one_to_two: DirectionalChannelInfo {
					src_node_id: node3.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (6 << 8) | 1,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				}, two_to_one: DirectionalChannelInfo {
					src_node_id: node5.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (6 << 8) | 2,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				},
			});
			network.channels.insert(NetworkMap::get_key(11, zero_hash.clone()), ChannelInfo {
				features: GlobalFeatures::new(),
				one_to_two: DirectionalChannelInfo {
					src_node_id: node5.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (11 << 8) | 1,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				}, two_to_one: DirectionalChannelInfo {
					src_node_id: node4.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (11 << 8) | 2,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				},
			});
			network.nodes.insert(node6.clone(), NodeInfo {
				channels: vec!(NetworkMap::get_key(7, zero_hash.clone())),
				lowest_inbound_channel_fee_base_msat: 0,
				lowest_inbound_channel_fee_proportional_millionths: 0,
				features: GlobalFeatures::new(),
				last_update: 1,
				rgb: [0; 3],
				alias: [0; 32],
				addresses: Vec::new(),
			});
			network.channels.insert(NetworkMap::get_key(7, zero_hash.clone()), ChannelInfo {
				features: GlobalFeatures::new(),
				one_to_two: DirectionalChannelInfo {
					src_node_id: node3.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (7 << 8) | 1,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 1000000,
				}, two_to_one: DirectionalChannelInfo {
					src_node_id: node6.clone(),
					last_update: 0,
					enabled: true,
					cltv_expiry_delta: (7 << 8) | 2,
					htlc_minimum_msat: 0,
					fee_base_msat: 0,
					fee_proportional_millionths: 0,
				},
			});
		}

		{ // Simple route to 3 via 2
			let route = router.get_route(&node3, &Vec::new(), 100, 42).unwrap();
			assert_eq!(route.hops.len(), 2);

			assert_eq!(route.hops[0].pubkey, node2);
			assert_eq!(route.hops[0].short_channel_id, 2);
			assert_eq!(route.hops[0].fee_msat, 100);
			assert_eq!(route.hops[0].cltv_expiry_delta, (4 << 8) | 1);

			assert_eq!(route.hops[1].pubkey, node3);
			assert_eq!(route.hops[1].short_channel_id, 4);
			assert_eq!(route.hops[1].fee_msat, 100);
			assert_eq!(route.hops[1].cltv_expiry_delta, 42);
		}

		{ // Route to 1 via 2 and 3 because our channel to 1 is disabled
			let route = router.get_route(&node1, &Vec::new(), 100, 42).unwrap();
			assert_eq!(route.hops.len(), 3);

			assert_eq!(route.hops[0].pubkey, node2);
			assert_eq!(route.hops[0].short_channel_id, 2);
			assert_eq!(route.hops[0].fee_msat, 200);
			assert_eq!(route.hops[0].cltv_expiry_delta, (4 << 8) | 1);

			assert_eq!(route.hops[1].pubkey, node3);
			assert_eq!(route.hops[1].short_channel_id, 4);
			assert_eq!(route.hops[1].fee_msat, 100);
			assert_eq!(route.hops[1].cltv_expiry_delta, (3 << 8) | 2);

			assert_eq!(route.hops[2].pubkey, node1);
			assert_eq!(route.hops[2].short_channel_id, 3);
			assert_eq!(route.hops[2].fee_msat, 100);
			assert_eq!(route.hops[2].cltv_expiry_delta, 42);
		}

		let mut last_hops = vec!(RouteHint {
				src_node_id: node4.clone(),
				short_channel_id: 8,
				fee_base_msat: 0,
				fee_proportional_millionths: 0,
				cltv_expiry_delta: (8 << 8) | 1,
				htlc_minimum_msat: 0,
			}, RouteHint {
				src_node_id: node5.clone(),
				short_channel_id: 9,
				fee_base_msat: 1001,
				fee_proportional_millionths: 0,
				cltv_expiry_delta: (9 << 8) | 1,
				htlc_minimum_msat: 0,
			}, RouteHint {
				src_node_id: node6.clone(),
				short_channel_id: 10,
				fee_base_msat: 0,
				fee_proportional_millionths: 0,
				cltv_expiry_delta: (10 << 8) | 1,
				htlc_minimum_msat: 0,
			});

		{ // Simple test across 2, 3, 5, and 4 via a last_hop channel
			let route = router.get_route(&node7, &last_hops, 100, 42).unwrap();
			assert_eq!(route.hops.len(), 5);

			assert_eq!(route.hops[0].pubkey, node2);
			assert_eq!(route.hops[0].short_channel_id, 2);
			assert_eq!(route.hops[0].fee_msat, 100);
			assert_eq!(route.hops[0].cltv_expiry_delta, (4 << 8) | 1);

			assert_eq!(route.hops[1].pubkey, node3);
			assert_eq!(route.hops[1].short_channel_id, 4);
			assert_eq!(route.hops[1].fee_msat, 0);
			assert_eq!(route.hops[1].cltv_expiry_delta, (6 << 8) | 1);

			assert_eq!(route.hops[2].pubkey, node5);
			assert_eq!(route.hops[2].short_channel_id, 6);
			assert_eq!(route.hops[2].fee_msat, 0);
			assert_eq!(route.hops[2].cltv_expiry_delta, (11 << 8) | 1);

			assert_eq!(route.hops[3].pubkey, node4);
			assert_eq!(route.hops[3].short_channel_id, 11);
			assert_eq!(route.hops[3].fee_msat, 0);
			assert_eq!(route.hops[3].cltv_expiry_delta, (8 << 8) | 1);

			assert_eq!(route.hops[4].pubkey, node7);
			assert_eq!(route.hops[4].short_channel_id, 8);
			assert_eq!(route.hops[4].fee_msat, 100);
			assert_eq!(route.hops[4].cltv_expiry_delta, 42);
		}

		last_hops[0].fee_base_msat = 1000;

		{ // Revert to via 6 as the fee on 8 goes up
			let route = router.get_route(&node7, &last_hops, 100, 42).unwrap();
			assert_eq!(route.hops.len(), 4);

			assert_eq!(route.hops[0].pubkey, node2);
			assert_eq!(route.hops[0].short_channel_id, 2);
			assert_eq!(route.hops[0].fee_msat, 200); // fee increased as its % of value transferred across node
			assert_eq!(route.hops[0].cltv_expiry_delta, (4 << 8) | 1);

			assert_eq!(route.hops[1].pubkey, node3);
			assert_eq!(route.hops[1].short_channel_id, 4);
			assert_eq!(route.hops[1].fee_msat, 100);
			assert_eq!(route.hops[1].cltv_expiry_delta, (7 << 8) | 1);

			assert_eq!(route.hops[2].pubkey, node6);
			assert_eq!(route.hops[2].short_channel_id, 7);
			assert_eq!(route.hops[2].fee_msat, 0);
			assert_eq!(route.hops[2].cltv_expiry_delta, (10 << 8) | 1);

			assert_eq!(route.hops[3].pubkey, node7);
			assert_eq!(route.hops[3].short_channel_id, 10);
			assert_eq!(route.hops[3].fee_msat, 100);
			assert_eq!(route.hops[3].cltv_expiry_delta, 42);
		}

		{ // ...but still use 8 for larger payments as 6 has a variable feerate
			let route = router.get_route(&node7, &last_hops, 2000, 42).unwrap();
			assert_eq!(route.hops.len(), 5);

			assert_eq!(route.hops[0].pubkey, node2);
			assert_eq!(route.hops[0].short_channel_id, 2);
			assert_eq!(route.hops[0].fee_msat, 3000);
			assert_eq!(route.hops[0].cltv_expiry_delta, (4 << 8) | 1);

			assert_eq!(route.hops[1].pubkey, node3);
			assert_eq!(route.hops[1].short_channel_id, 4);
			assert_eq!(route.hops[1].fee_msat, 0);
			assert_eq!(route.hops[1].cltv_expiry_delta, (6 << 8) | 1);

			assert_eq!(route.hops[2].pubkey, node5);
			assert_eq!(route.hops[2].short_channel_id, 6);
			assert_eq!(route.hops[2].fee_msat, 0);
			assert_eq!(route.hops[2].cltv_expiry_delta, (11 << 8) | 1);

			assert_eq!(route.hops[3].pubkey, node4);
			assert_eq!(route.hops[3].short_channel_id, 11);
			assert_eq!(route.hops[3].fee_msat, 1000);
			assert_eq!(route.hops[3].cltv_expiry_delta, (8 << 8) | 1);

			assert_eq!(route.hops[4].pubkey, node7);
			assert_eq!(route.hops[4].short_channel_id, 8);
			assert_eq!(route.hops[4].fee_msat, 2000);
			assert_eq!(route.hops[4].cltv_expiry_delta, 42);
		}
	}
}
