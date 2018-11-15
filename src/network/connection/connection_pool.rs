use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio;

use crate::network;
use crate::network::address::net_address::{NetAddress, NetAddressType};
use crate::network::address::peer_address::PeerAddress;
use crate::network::connection::NetworkConnection;
use crate::network::Protocol;

use super::close_type::CloseType;
use super::connection_info::{ConnectionInfo, ConnectionState};
use std::collections::HashSet;
use std::collections::LinkedList;
use crate::network::Peer;

pub struct ConnectionPool {
    connections: SparseVec<ConnectionInfo>,
    connections_by_peer_address: HashMap<Arc<PeerAddress>, usize>,
    connections_by_net_address: HashMap<NetAddress, HashSet<usize>>,
    connections_by_subnet: HashMap<NetAddress, HashSet<usize>>,

    peer_count_ws: usize,
    peer_count_wss: usize,
    peer_count_rtc: usize,
    peer_count_dumb: usize,

    peer_count_full: usize,
    peer_count_light: usize,
    peer_count_nano: usize,

    peer_count_outbound: usize,
    peer_count_full_ws_outbound: usize,

    connecting_count: usize,

    inbound_count: usize,

    allow_inbound_connections: bool,
    allow_inbound_exchange: bool,

    banned_ips: HashMap<NetAddress, SystemTime>,
}

impl ConnectionPool {
    const DEFAULT_BAN_TIME: u64 = 1000 * 60 * 10; // seconds

    /// Initiates a outbound connection.
    pub fn connect_outbound(&mut self, peer_address: &PeerAddress) -> bool {
        // All checks in one step.
        if !self.check_outbound_connection_request(peer_address) {
            return false;
        }

        // Connection request accepted.

        // Create fresh ConnectionInfo instance.
        let connection_id = self.add(ConnectionInfo::outbound(peer_address.clone()));

        // Choose connector type and call.
        let mut connecting = false;
        match peer_address.protocol() {
            Protocol::Wss => {
                // TODO
                connecting = true;
            },
            Protocol::Ws => {
                // TODO
                connecting = true;
            },
            _ => {
                unreachable!("Cannot connect to non-WS(S) nodes.");
            },
        }

        if connecting {
            self.connecting_count += 1;
        } else {
            self.remove(connection_id);
            debug!("Outbound attempt not connecting: {:?}", peer_address);
            return false;
        }

        return true;
    }

    /// Get the connection info for a peer address.
    pub fn get_connection_by_peer_address(&self, peer_address: &PeerAddress) -> Option<&ConnectionInfo> {
        Some(self.connections.get(*self.connections_by_peer_address.get(peer_address)?).expect("Missing connection"))
    }

    /// Get the connection info for a peer address as a mutable borrow.
    pub fn get_connection_by_peer_address_mut(&mut self, peer_address: &PeerAddress) -> Option<&mut ConnectionInfo> {
        Some(self.connections.get_mut(*self.connections_by_peer_address.get(peer_address)?).expect("Missing connection"))
    }

    /// Get a list of connection info for a net address.
    pub fn get_connections_by_net_address(&self, net_address: &NetAddress) -> Option<Vec<&ConnectionInfo>> {
        self.connections_by_net_address.get(net_address).map(|s| {
            s.iter().map(|i| self.connections.get(*i).expect("Missing connection")).collect()
        })
    }

    /// Get the number of connections for a net address.
    pub fn get_num_connections_by_net_address(&self, net_address: &NetAddress) -> usize {
        self.connections_by_net_address.get(net_address).map_or(0, |s| s.len())
    }

    /// Get a list of connection info for a subnet.
    pub fn get_connections_by_subnet(&self, net_address: &NetAddress) -> Option<Vec<&ConnectionInfo>> {
        self.connections_by_subnet.get(&ConnectionPool::get_subnet_address(net_address)).map(|s| {
            s.iter().map(|i| self.connections.get(*i).expect("Missing connection")).collect()
        })
    }

    /// Get the number of connections for a subnet.
    pub fn get_num_connections_by_subnet(&self, net_address: &NetAddress) -> usize {
        self.connections_by_subnet.get(&ConnectionPool::get_subnet_address(net_address)).map_or(0, |s| s.len())
    }

    /// Retrieve a list of connection info for all outbound connections into a subnet.
    pub fn get_outbound_connections_by_subnet(&self, net_address: &NetAddress) -> Option<Vec<&ConnectionInfo>> {
        self.get_connections_by_subnet(net_address)
            .map(|mut v| {
                v.retain(|info| {
                    if let Some(network_connection) = info.network_connection() {
                        network_connection.outbound()
                    } else {
                        false
                    }
                });
                v
            })
    }

    /// Retrieve the number of connections for all outbound connections into a subnet.
    pub fn get_num_outbound_connections_by_subnet(&self, net_address: &NetAddress) -> usize {
        self.get_outbound_connections_by_subnet(net_address).map_or(0, |v| v.len())
    }

    /// Close a connection.
    fn close(info: &ConnectionInfo, ty: CloseType) {
        if let Some(network_connection) = info.network_connection() {
            tokio::spawn(network_connection.close(ty));
        }
    }

    /// Checks the validity of a connection.
    fn check_connection(&self, connection_id: usize) -> bool {
        let info = self.connections.get(connection_id).unwrap();
        let conn = info.network_connection();
        assert!(conn.is_some(), "Connection must be established");
        let conn = conn.unwrap();

        // Close connection if we currently do not allow inbound connections.
        // TODO WebRTC connections are exempt.
        if conn.inbound() && !self.allow_inbound_connections {
            ConnectionPool::close(info, CloseType::InboundConnectionsBlocked);
            return false;
        }

        let net_address = conn.net_address();
        if net_address.is_reliable() {
            // Close connection if peer's IP is banned.
            if self.is_ip_banned(&net_address) {
                ConnectionPool::close(info, CloseType::BannedIp);
                return false;
            }

            // Close connection if we have too many connections to the peer's IP address.
            if self.get_num_connections_by_net_address(&net_address) > network::PEER_COUNT_PER_IP_MAX {
                ConnectionPool::close(info, CloseType::ConnectionLimitPerIp);
                return false;
            }

            // Close connection if we have too many connections to the peer's subnet.
            if self.get_num_connections_by_subnet(&net_address) > network::INBOUND_PEER_COUNT_PER_SUBNET_MAX {
                ConnectionPool::close(info, CloseType::ConnectionLimitPerIp);
                return false;
            }
        }

        // Reject peer if we have reached max peer count.
        // There are two exceptions to this: outbound connections
        // and inbound connections with inbound exchange set.
        if self.peer_count() >= network::PEER_COUNT_MAX
            && !conn.outbound()
            && !(conn.inbound() && self.allow_inbound_exchange) {

            ConnectionPool::close(info, CloseType::MaxPeerCountReached);
            return false;
        }
        return true;
    }

    /// Callback upon connection establishment.
    fn on_connection(&mut self, connection: NetworkConnection) {
        let connection_id;
        if connection.outbound() {
            self.connecting_count = self.connecting_count.checked_sub(1).expect("connecting_count < 0");

            let peer_address = connection.peer_address().expect("Outbound connection without peer address");
            connection_id = *self.connections_by_peer_address.get(&peer_address).expect("Outbound connection without entry in connection pool");

            assert_eq!(self.connections.get(connection_id).unwrap().state(), ConnectionState::Connecting, "Expected state to be connecting ({:?})", peer_address);

            // Set peerConnection to CONNECTED state.
            self.connections.get_mut(connection_id).unwrap().set_network_connection(connection);
        } else {
            // Add connection (without having obtained peer address).
            connection_id = self.add(ConnectionInfo::inbound(connection));
            self.inbound_count += 1;
        }

        // TODO Register close listener early to clean up correctly in case _checkConnection() closes the connection.

        if !self.check_connection(connection_id) {
            return;
        }
    }

    /// Checks the validity of a handshake.
    fn check_handshake(&mut self, connection_id: usize, peer: &Peer) -> bool {
        let info = self.connections.get(connection_id).unwrap();

        // Close connection if peer's address is banned.
        // TODO

        // Duplicate/simultaneous connection check (post version):
        let stored_connection_id = self.connections_by_peer_address.get(&peer.peer_address());
        if let Some(stored_connection_id) = stored_connection_id {
            if *stored_connection_id != connection_id {
                // If we already have an established connection to this peer, close this connection.
                let stored_connection = self.connections.get(*stored_connection_id).expect("Missing connection");
                if stored_connection.state() == ConnectionState::Established {
                    ConnectionPool::close(info, CloseType::DuplicateConnection);
                    return false;
                }
            }
        }

        // Close connection if we have too many dumb connections.
        if peer.peer_address().protocol() == Protocol::Dumb && self.peer_count_dumb >= network::PEER_COUNT_DUMB_MAX {
            ConnectionPool::close(info, CloseType::ConnectionLimitDumb);
            return false;
        }

        // Set peerConnection to NEGOTIATING state.
        self.connections.get_mut(connection_id).unwrap().negotiating();

        return false;
    }

    /// Callback during handshake.
    fn on_handshake(&mut self, connection_id: usize, peer: Peer) { // TODO Arc<RwLock<Peer>>?
        let info = self.connections.get(connection_id).expect("Missing connection");
        let network_connection = info.network_connection().unwrap();

        if network_connection.inbound() {
            // Re-check allowInboundExchange as it might have changed.
            if self.peer_count() >= network::PEER_COUNT_MAX && !self.allow_inbound_exchange {
                ConnectionPool::close(info, CloseType::MaxPeerCountReached);
                return;
            }

            // Duplicate/simultaneous connection check (post handshake):
            let stored_connection_id = self.connections_by_peer_address.get(&peer.peer_address());
            if let Some(stored_connection_id) = stored_connection_id {
                if *stored_connection_id != connection_id {
                    let stored_connection = self.connections.get(*stored_connection_id).expect("Missing connection");
                    match stored_connection.state() {
                        ConnectionState::Connecting => {
                            // Abort the stored connection attempt and accept this connection.
                            let protocol = peer.peer_address().protocol();
                            assert!(protocol == Protocol::Wss || protocol == Protocol::Ws, "Duplicate connection to non-WS node");
                            debug!("Aborting connection attempt to {:?}, simultaneous connection succeeded", peer.peer_address());

                            // TODO abort connecting
                            assert!(self.get_connection_by_peer_address(&peer.peer_address()).is_none(), "ConnectionInfo not removed");
                        },
                        ConnectionState::Established => {
                            // If we have another established connection to this peer, close this connection.
                            ConnectionPool::close(info, CloseType::DuplicateConnection);
                            return;
                        },
                        ConnectionState::Negotiating => {
                            // The peer with the lower peerId accepts this connection and closes his stored connection.
                            // TODO get own PeerId and compare
                            // if <self>.peer_address().peer_id() < peer.peer_address().peer_id() {
                            if true {
                                ConnectionPool::close(stored_connection, CloseType::SimultaneousConnection);
                                assert!(self.get_connection_by_peer_address(&peer.peer_address()).is_none(), "ConnectionInfo not removed");
                            } else {
                                // The peer with the higher peerId closes this connection and keeps his stored connection.
                                ConnectionPool::close(info, CloseType::SimultaneousConnection);
                            }
                        },
                        _ => {
                            // Accept this connection and close the stored connection.
                            ConnectionPool::close(stored_connection, CloseType::SimultaneousConnection);
                            assert!(self.get_connection_by_peer_address(&peer.peer_address()).is_none(), "ConnectionInfo not removed");
                        },
                    }
                }
            }

            assert!(self.get_connection_by_peer_address(&peer.peer_address()).is_none(), "ConnectionInfo already exists");
            self.connections.get_mut(connection_id).unwrap().set_peer_address(peer.peer_address());
            self.add_peer_address(connection_id, peer.peer_address());

            self.inbound_count = self.inbound_count.checked_sub(1).expect("inbound_count < 0");
        }

        // Handshake accepted.

        // Check if we need to recycle a connection.
        if self.peer_count() >= network::PEER_COUNT_MAX {
            // TODO fire event
        }

        // Set ConnectionInfo to Established state.
        self.connections.get_mut(connection_id).unwrap().set_peer(peer.clone()); // TODO do we need a clone here?

        if let Some(net_address) = peer.net_address() {
            // The HashSet takes care of only inserting it once.
            self.add_net_address(connection_id, &net_address);
        }

        self.update_connected_peer_count(connection_id, PeerCountUpdate::Add);

        // TODO Setup signal forwarding.

        // TODO Mark address as established.

        // TODO Let listeners know about this peer.

        // TODO Let listeners know that the peers changed.

        debug!("[PEER-JOINED] {:?} {:?} (version={:?}, services={:?}, headHash={:?})", peer.peer_address(), peer.net_address(), peer.version, peer.peer_address().services, peer.head_hash);
    }

    /// Callback upon closing of connection.
    fn on_close(&mut self, connection_id: usize, ty: CloseType) {
        // Only propagate the close type (i.e. track fails/bans) if the peerAddress is set.
        // This is true for
        // - all outbound connections
        // - inbound connections post handshake (peerAddress is verified)
        let info = self.connections.get(connection_id).unwrap();
        if let Some(peer_address) = info.peer_address() {
            // TODO propagate to address book
        }

        let mut info = self.remove(connection_id);

        // Check if the handshake with this peer has completed.
        if info.state() == ConnectionState::Established {
            let net_address = info.network_connection().map(|p| p.net_address());
            // If closing is due to a ban, also ban the IP
            if ty.is_banning_type() {
                if let Some(ref net_address) = net_address {
                    self.ban_ip(net_address);
                }
            }

            self.update_connected_peer_count(connection_id, PeerCountUpdate::Remove);

            // TODO Tell listeners that this peer has gone away.

            // TODO Let listeners know that the peers changed.

            debug!("[PEER-LEFT] {:?} {:?} (version={:?}, closeType={:?})", info.peer_address(), net_address, info.peer().map(|p| p.version), ty);
        } else {
            match info.network_connection().map(|n| n.inbound()) {
                Some(true) => {
                    self.inbound_count.checked_sub(1).expect("inbound_count < 0");
                    debug!("Inbound connection #{:?} closed pre-handshake: {:?}", connection_id, ty);
                },
                Some(false) => {
                    debug!("Connection #{:?} to {:?} closed pre-handshake: {:?}", connection_id, info.peer_address(), ty);
                    // TODO fire connect-error
                },
                _ => unreachable!("Invalid state, closing connection with network connection not set"),
            }
        }

        // TODO Let listeners know about this closing.

        // Set the peer connection to closed state.
        info.close();
    }

    /// Total peer count.
    pub fn peer_count(&self) -> usize {
        self.peer_count_ws + self.peer_count_wss + self.peer_count_rtc + self.peer_count_dumb
    }

    /// Bans an IP address.
    fn ban_ip(&mut self, net_address: &NetAddress) {
        if net_address.is_reliable() {
            warn!("Banning ip {:?}", net_address);
            let banned_address = if net_address.get_type() == NetAddressType::IPv4 {
                net_address.clone()
            } else {
                net_address.subnet(64)
            };
            let unban_time = SystemTime::now() + Duration::new(ConnectionPool::DEFAULT_BAN_TIME, 0);
            self.banned_ips.insert(banned_address, unban_time);
        }
    }

    fn is_ip_banned(&self, net_address: &NetAddress) -> bool {
        !net_address.is_pseudo() && self.banned_ips.contains_key(net_address)
    }

    /// Called to regularly unban IPs.
    fn check_unban_ips(&mut self) {
        let mut now = SystemTime::now();
        self.banned_ips.retain(|net_address, unban_time| {
            unban_time > &mut now
        });
    }

    // TODO fn on_connect_error(peer_address: PeerAddress, reason: )

    /// Updates the number of connected peers.
    fn update_connected_peer_count(&mut self, connection_id: usize, update: PeerCountUpdate) {
        // We assume the connection to be present and having a valid peer address/network connection.
        let info = self.connections.get(connection_id).unwrap();
        let peer_address = info.peer_address().unwrap();
        let network_connection = info.network_connection().unwrap();

        match peer_address.protocol() {
            Protocol::Wss => self.peer_count_wss = match update {
                PeerCountUpdate::Add => self.peer_count_wss + 1,
                PeerCountUpdate::Remove => self.peer_count_wss.checked_sub(1).expect("peer_count_wss < 0"),
            },
            Protocol::Ws => self.peer_count_ws = match update {
                PeerCountUpdate::Add => self.peer_count_ws + 1,
                PeerCountUpdate::Remove => self.peer_count_ws.checked_sub(1).expect("peer_count_ws < 0"),
            },
            Protocol::Rtc => self.peer_count_rtc = match update {
                PeerCountUpdate::Add => self.peer_count_rtc + 1,
                PeerCountUpdate::Remove => self.peer_count_rtc.checked_sub(1).expect("peer_count_rtc < 0"),
            },
            Protocol::Dumb => self.peer_count_dumb = match update {
                PeerCountUpdate::Add => self.peer_count_dumb + 1,
                PeerCountUpdate::Remove => self.peer_count_dumb.checked_sub(1).expect("peer_count_dumb < 0"),
            },
        }

        // TODO Check services.

        if network_connection.outbound() {
            self.peer_count_outbound = match update {
                PeerCountUpdate::Add => self.peer_count_outbound + 1,
                PeerCountUpdate::Remove => self.peer_count_outbound.checked_sub(1).expect("peer_count_outbound < 0"),
            };
            // TODO: check for WS full node
        }
    }

    /// Convert a net address into a subnet according to the configured bitmask.
    fn get_subnet_address(net_address: &NetAddress) -> NetAddress {
        let bit_mask = if net_address.get_type() == NetAddressType::IPv4 { network::IPV4_SUBNET_MASK } else { network::IPV6_SUBNET_MASK };
        net_address.subnet(bit_mask)
    }

    /// Check the validity of a outbound connection request (e.g. no duplicate connections).
    fn check_outbound_connection_request(&self, peer_address: &PeerAddress) -> bool {
        match peer_address.protocol() {
            Protocol::Wss => {},
            Protocol::Ws => {},
            _ => {
                warn!("Cannot connect to {} - unsupported protocol", peer_address);
                return false;
            },
        }

        // TODO check banned

        let info = self.get_connection_by_peer_address(peer_address);
        if let Some(info) = info {
            debug!("Duplicate connection to {}", peer_address);
            return false;
        }

        // Forbid connection if we have too many connections to the peer's IP address.
        if peer_address.net_address.is_reliable() {
            if self.get_num_connections_by_net_address(&peer_address.net_address) >= network::PEER_COUNT_PER_IP_MAX {
                debug!("Connection limit per IP ({}) reached", network::PEER_COUNT_PER_IP_MAX);
                return false;
            }

            if self.get_num_outbound_connections_by_subnet(&peer_address.net_address) >= network::OUTBOUND_PEER_COUNT_PER_SUBNET_MAX {
                debug!("Connection limit per IP ({}) reached", network::OUTBOUND_PEER_COUNT_PER_SUBNET_MAX);
                return false;
            }
        }

        return true;
    }

    /// Add a new connection to the connection pool.
    fn add(&mut self, info: ConnectionInfo) -> usize {
        let peer_address = info.peer_address();
        let connection_id = self.connections.insert(info);

        // Add to peer address map if available.
        if let Some(peer_address) = peer_address {
            self.connections_by_peer_address.insert(peer_address, connection_id);
        }
        connection_id
    }

    /// Add a new connection to the connection pool.
    fn add_peer_address(&mut self, connection_id: usize, peer_address: Arc<PeerAddress>) {
        // Add to peer address map.
        self.connections_by_peer_address.insert(peer_address, connection_id);
    }

    /// Remove a connection from the connection pool.
    fn remove(&mut self, connection_id: usize) -> ConnectionInfo {
        // TODO: Can we make sure that we never remove a connection twice?
        let info = self.connections.remove(connection_id).unwrap();

        if let Some(peer_address) = info.peer_address() {
            self.connections_by_peer_address.remove(&peer_address);
        }

        if let Some(network_connection) = info.network_connection() {
            self.remove_net_address(connection_id, &network_connection.net_address());
        }

        info
    }

    /// Adds the net address to a connection.
    fn add_net_address(&mut self, connection_id: usize, net_address: &NetAddress) {
        // Only add reliable netAddresses.
        if !net_address.is_reliable() {
            return;
        }

        self.connections_by_net_address.entry(net_address.clone())
            .or_insert_with(HashSet::new)
            .insert(connection_id);

        let subnet_address = ConnectionPool::get_subnet_address(net_address);
        self.connections_by_subnet.entry(subnet_address)
            .or_insert_with(HashSet::new)
            .insert(connection_id);
    }

    /// Removes the connection from net address specific maps.
    fn remove_net_address(&mut self, connection_id: usize, net_address: &NetAddress) {
        // Only add reliable netAddresses.
        if !net_address.is_reliable() {
            return;
        }

        if let Entry::Occupied(mut occupied) = self.connections_by_net_address.entry(net_address.clone()) {
            let is_empty = {
                let s = occupied.get_mut();

                s.remove(&connection_id);

                s.is_empty()
            };
            if is_empty {
                occupied.remove();
            }
        }

        let subnet_address = ConnectionPool::get_subnet_address(net_address);
        if let Entry::Occupied(mut occupied) = self.connections_by_subnet.entry(subnet_address) {
            let is_empty = {
                let s = occupied.get_mut();

                s.remove(&connection_id);

                s.is_empty()
            };
            if is_empty {
                occupied.remove();
            }
        }
    }
}

enum PeerCountUpdate {
    Add,
    Remove
}

/// This is a special vector implementation that has a O(1) remove function.
/// It never shrinks in size, but reuses available spaces as much as possible.
struct SparseVec<T> {
    inner: Vec<Option<T>>,
    free_indices: LinkedList<usize>,
}

impl<T> SparseVec<T> {
    pub fn new() -> Self {
        SparseVec {
            inner: Vec::new(),
            free_indices: LinkedList::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        SparseVec {
            inner: Vec::with_capacity(capacity),
            free_indices: LinkedList::new(),
        }
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.inner.get(index)?.as_ref()
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.inner.get_mut(index)?.as_mut()
    }

    pub fn remove(&mut self, index: usize) -> Option<T> {
        self.free_indices.push_back(index);
        self.inner.get_mut(index)?.take()
    }

    pub fn insert(&mut self, value: T) -> usize {
        if let Some(index) = self.free_indices.pop_front() {
            self.inner.get_mut(index).unwrap().get_or_insert(value);
            index
        } else {
            let index = self.inner.len();
            self.inner.push(Some(value));
            index
        }
    }
}