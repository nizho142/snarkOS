// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{message::*, peers::PeerQuality, ConnReader, ConnWriter, NetworkError, Node, Version};

use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Instant,
};

use parking_lot::Mutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

impl Node {
    ///
    /// Broadcasts updates with connected peers and maintains a permitted number of connected peers.
    ///
    pub async fn update_peers(&self) -> Result<(), NetworkError> {
        // Fetch the number of connected peers.
        let number_of_connected_peers = self.peer_book.read().number_of_connected_peers() as usize;
        trace!(
            "Connected to {} peer{}",
            number_of_connected_peers,
            if number_of_connected_peers == 1 { "" } else { "s" }
        );

        // Check that this node is not a bootnode.
        if !self.environment.is_bootnode() {
            // Check if this node server is below the permitted number of connected peers.
            let min_peers = self.environment.minimum_number_of_connected_peers() as usize;
            if number_of_connected_peers < min_peers {
                // Attempt to connect to the default bootnodes of the network.
                self.connect_to_bootnodes().await;

                // Attempt to connect to each disconnected peer saved in the peer book.
                self.connect_to_disconnected_peers(min_peers - number_of_connected_peers)
                    .await;

                // Broadcast a `GetPeers` message to request for more peers.
                self.broadcast_getpeers_requests().await;
            }
        }

        // Check if this node server is above the permitted number of connected peers.
        let max_peers = self.environment.maximum_number_of_connected_peers() as usize;
        if number_of_connected_peers > max_peers {
            let number_to_disconnect = number_of_connected_peers - max_peers;
            trace!(
                "Disconnecting from the most recent {} peers to maintain their permitted number",
                number_to_disconnect
            );

            let peer_book = self.peer_book.read();

            let mut connected = peer_book
                .connected_peers()
                .into_iter()
                .map(|(_, peer_info)| peer_info)
                .collect::<Vec<_>>();
            connected.sort_unstable_by_key(|info| info.last_connected());

            for _ in 0..number_to_disconnect {
                if let Some(peer_info) = connected.pop() {
                    let addr = peer_info.address();
                    let _ = self.disconnect_from_peer(addr);
                }
            }
        }

        if number_of_connected_peers != 0 {
            // Send a `Ping` to every connected peer.
            self.broadcast_pings().await;

            // Store the peer book to storage.
            // self.save_peer_book_to_storage()?;
        }

        Ok(())
    }

    ///
    /// Returns the `SocketAddr` of the last seen peer to be used as a sync node, or `None`.
    ///
    pub fn last_seen(&self) -> Option<SocketAddr> {
        if let Some((&socket_address, _)) = self
            .peer_book
            .read()
            .connected_peers()
            .iter()
            .max_by(|a, b| a.1.last_seen().cmp(&b.1.last_seen()))
        {
            Some(socket_address)
        } else {
            None
        }
    }

    async fn initiate_connection(&self, remote_address: SocketAddr) -> Result<(), NetworkError> {
        let own_address = self.local_address().unwrap(); // must be known by now
        if remote_address == own_address
            || ((remote_address.ip().is_unspecified() || remote_address.ip().is_loopback())
                && remote_address.port() == own_address.port())
        {
            return Err(NetworkError::SelfConnectAttempt);
        }
        if self.peer_book.read().is_connecting(remote_address) {
            return Err(NetworkError::PeerAlreadyConnecting);
        }
        if self.peer_book.read().is_connected(remote_address) {
            return Err(NetworkError::PeerAlreadyConnected);
        }

        self.peer_book.write().set_connecting(remote_address)?;

        // open the connection
        let stream = TcpStream::connect(remote_address).await?;
        let (mut reader, mut writer) = stream.into_split();

        let builder = snow::Builder::with_resolver(
            crate::HANDSHAKE_PATTERN
                .parse()
                .expect("Invalid noise handshake pattern!"),
            Box::new(snow::resolvers::SodiumResolver),
        );
        let static_key = builder.generate_keypair().map_err(NetworkError::Noise)?.private;
        let noise_builder = builder.local_private_key(&static_key).psk(3, crate::HANDSHAKE_PSK);
        let mut noise = noise_builder.build_initiator().map_err(NetworkError::Noise)?;
        let mut buffer: Box<[u8]> = vec![0u8; crate::MAX_MESSAGE_SIZE].into();
        let mut buf = [0u8; crate::NOISE_BUF_LEN]; // a temporary intermediate buffer to decrypt from

        // -> e
        let len = noise.write_message(&[], &mut buffer).map_err(NetworkError::Noise)?;
        println!("len: {}", len);
        writer.write_all(&[len as u8]).await?;
        writer.write_all(&buffer[..len]).await?;
        trace!("sent e (XX handshake part 1/3)");

        // <- e, ee, s, es
        reader.read_exact(&mut buf[..1]).await?;
        let len = buf[0] as usize;
        let len = reader.read_exact(&mut buf[..len]).await?;
        let len = noise
            .read_message(&buf[..len], &mut buffer)
            .map_err(|_| NetworkError::InvalidHandshake)?;
        let _peer_version = Version::deserialize(&buffer[..len]).map_err(|_| NetworkError::InvalidHandshake)?;
        trace!("received e, ee, s, es (XX handshake part 2/3)");

        // -> s, se, psk
        let own_version = Version::serialize(&Version::new(1u64, own_address.port())).unwrap();
        let len = noise
            .write_message(&own_version, &mut buffer)
            .map_err(NetworkError::Noise)?;
        writer.write_all(&[len as u8]).await?;
        writer.write_all(&buffer[..len]).await?;
        trace!("sent s, se, psk (XX handshake part 3/3)");

        let noise = Arc::new(Mutex::new(noise.into_transport_mode().map_err(NetworkError::Noise)?));
        let writer = ConnWriter::new(remote_address, writer, buffer.clone(), Arc::clone(&noise));
        let mut reader = ConnReader::new(remote_address, reader, buffer, noise);

        // spawn the inbound loop
        let inbound = self.inbound.clone();
        tokio::spawn(async move {
            inbound.listen_for_messages(&mut reader).await;
        });

        // save the outbound channel
        self.outbound.channels.write().insert(remote_address, Arc::new(writer));

        self.peer_book.write().set_connected(remote_address, None)
    }

    ///
    /// Broadcasts a connection request to all default bootnodes of the network.
    ///
    /// This function attempts to reconnect this node server with any bootnode peer
    /// that this node may have failed to connect to.
    ///
    /// This function filters out any bootnode peers the node server is already connected to.
    ///
    async fn connect_to_bootnodes(&self) {
        trace!("Connecting to default bootnodes");

        // Fetch the current connected peers of this node.
        let connected_peers = self.peer_book.read().connected_peers().clone();

        // Iterate through each bootnode address and attempt a connection request.
        for bootnode_address in self
            .environment
            .bootnodes()
            .iter()
            .filter(|addr| !connected_peers.contains_key(addr))
            .copied()
        {
            if let Err(e) = self.initiate_connection(bootnode_address).await {
                warn!("Couldn't connect to bootnode {}: {}", bootnode_address, e);
                let _ = self.disconnect_from_peer(bootnode_address);
            }
        }
    }

    /// Broadcasts a connection request to all disconnected peers.
    async fn connect_to_disconnected_peers(&self, count: usize) {
        trace!("Connecting to disconnected peers");

        // Iterate through each connected peer and attempts a connection request.
        let disconnected_peers = self.peer_book.read().disconnected_peers().clone();
        for remote_address in disconnected_peers.keys().take(count).copied() {
            if let Err(e) = self.initiate_connection(remote_address).await {
                trace!("Couldn't connect to the disconnected peer {}: {}", remote_address, e);
                let _ = self.disconnect_from_peer(remote_address);
            }
        }
    }

    /// Broadcasts a `Ping` message to all connected peers.
    async fn broadcast_pings(&self) {
        trace!("Broadcasting Ping messages");

        let current_block_height = self.consensus().current_block_height();
        let connected_peers = self.peer_book.read().connected_peers().clone();
        for (remote_address, _) in connected_peers {
            self.sending_ping(remote_address);

            self.outbound
                .send_request(Message::new(
                    Direction::Outbound(remote_address),
                    Payload::Ping(current_block_height),
                ))
                .await;
        }
    }

    /// Broadcasts a `GetPeers` message to all connected peers to request for more peers.
    async fn broadcast_getpeers_requests(&self) {
        trace!("Sending GetPeers requests to connected peers");

        let connected_peers = self.peer_book.read().connected_peers().clone();
        for (remote_address, _) in connected_peers {
            self.outbound
                .send_request(Message::new(Direction::Outbound(remote_address), Payload::GetPeers))
                .await;

            // // Fetch the connection channel.
            // if let Some(channel) = self.get_channel(&remote_address) {
            //     // Broadcast the message over the channel.
            //     if let Err(_) = channel.write(&GetPeers).await {
            //         // Disconnect from the peer if the message fails to send.
            //         self.disconnect_from_peer(&remote_address).await?;
            //     }
            // } else {
            //     // Disconnect from the peer if the channel is not active.
            //     self.disconnect_from_peer(&remote_address).await?;
            // }
        }
    }

    fn peer_quality(&self, addr: SocketAddr) -> Option<Arc<PeerQuality>> {
        self.peer_book
            .read()
            .connected_peers()
            .get(&addr)
            .map(|peer| Arc::clone(&peer.quality))
    }

    fn sending_ping(&self, target: SocketAddr) {
        if let Some(quality) = self.peer_quality(target) {
            let timestamp = Instant::now();
            *quality.last_ping_sent.lock() = Some(timestamp);
            quality.expecting_pong.store(true, Ordering::SeqCst);
        } else {
            // shouldn't occur, but just in case
            warn!("Tried to send a Ping to an unknown peer: {}!", target);
        }
    }

    /// Handles an incoming `Pong` message.
    pub fn received_pong(&self, source: SocketAddr) {
        if let Some(quality) = self.peer_quality(source) {
            if quality.expecting_pong.load(Ordering::SeqCst) {
                let ping_sent = quality.last_ping_sent.lock().unwrap();
                let rtt = ping_sent.elapsed().as_millis() as u64;
                quality.rtt_ms.store(rtt, Ordering::SeqCst);
                quality.expecting_pong.store(false, Ordering::SeqCst);
            } else {
                quality.failures.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            // shouldn't occur, but just in case
            warn!("Received a Pong from an unknown peer: {}!", source);
        }
    }

    ///
    /// Updates the last seen timestamp of this peer to the current time.
    ///
    #[inline]
    pub fn update_last_seen(&self, addr: SocketAddr) {
        if let Some(ref quality) = self.peer_quality(addr) {
            *quality.last_seen.write() = Some(chrono::Utc::now());
        } else {
            warn!("Attempted to update state of a peer that's not connected: {}", addr);
        }
    }

    /// TODO (howardwu): Implement manual serializers and deserializers to prevent forward breakage
    ///  when the PeerBook or PeerInfo struct fields change.
    ///
    /// Stores the current peer book to the given storage object.
    ///
    /// This function checks that this node is not connected to itself,
    /// and proceeds to serialize the peer book into a byte vector for storage.
    ///
    // #[inline]
    // fn save_peer_book_to_storage(&self) -> Result<(), NetworkError> {
    //     // Serialize the peer book.
    //     let serialized_peer_book = bincode::serialize(&*self.peer_book.read())?;

    //     // Save the serialized peer book to storage.
    //     self.environment
    //         .storage()
    //         .write()
    //         .save_peer_book_to_storage(serialized_peer_book)?;

    //     Ok(())
    // }

    /// Registers that the given number of blocks is expected as part of syncing with a peer.
    pub fn expecting_sync_blocks(&self, addr: SocketAddr, count: usize) {
        if let Some(ref pq) = self.peer_quality(addr) {
            pq.remaining_sync_blocks.store(count as u16, Ordering::SeqCst);
        } else {
            error!("Peer for expecting_sync_blocks purposes not found!");
        }
    }

    /// Registers the receipt of a sync block from a peer; returns `true` when finished syncing.
    pub fn got_sync_block(&self, addr: SocketAddr) -> bool {
        if let Some(ref pq) = self.peer_quality(addr) {
            pq.remaining_sync_blocks.fetch_sub(1, Ordering::SeqCst) == 1
        } else {
            error!("Peer for got_sync_block purposes not found!");
            true
        }
    }

    /// Checks whether the current peer is involved in a block syncing process.
    pub fn is_syncing_blocks(&self, addr: SocketAddr) -> bool {
        if let Some(ref pq) = self.peer_quality(addr) {
            pq.remaining_sync_blocks.load(Ordering::SeqCst) != 0
        } else {
            error!("Peer for got_sync_block purposes not found!");
            false
        }
    }

    /// TODO (howardwu): Add logic to remove the active channels
    ///  and handshakes of the peer from this struct.
    /// Sets the given remote address in the peer book as disconnected from this node server.
    ///
    #[inline]
    pub(crate) fn disconnect_from_peer(&self, remote_address: SocketAddr) -> Result<(), NetworkError> {
        debug!("Disconnecting from {}", remote_address);

        if self.is_syncing_blocks(remote_address) {
            self.consensus().finished_syncing_blocks();
        }

        if let Some(handle) = self.inbound.tasks.lock().remove(&remote_address) {
            handle.abort();
        };
        self.outbound.channels.write().remove(&remote_address);

        self.peer_book.write().set_disconnected(remote_address)
        // TODO (howardwu): Attempt to blindly send disconnect message to peer.
    }

    pub(crate) async fn send_get_peers(&self, remote_address: SocketAddr) {
        // TODO (howardwu): Simplify this and parallelize this with Rayon.
        // Broadcast the sanitized list of connected peers back to requesting peer.
        let mut peers = Vec::new();
        for peer_address in self.peer_book.read().connected_peers().keys().copied() {
            // Skip the iteration if the requesting peer that we're sending the response to
            // appears in the list of peers.
            if peer_address == remote_address {
                continue;
            }
            peers.push(peer_address);
        }
        self.outbound
            .send_request(Message::new(Direction::Outbound(remote_address), Payload::Peers(peers)))
            .await;
    }

    /// A miner has sent their list of peer addresses.
    /// Add all new/updated addresses to our disconnected.
    /// The connection handler will be responsible for sending out handshake requests to them.
    pub(crate) fn process_inbound_peers(&self, peers: Vec<SocketAddr>) {
        // TODO (howardwu): Simplify this and parallelize this with Rayon.
        // Process all of the peers sent in the message,
        // by informing the peer book of that we found peers.
        let local_address = self.environment.local_address().unwrap(); // the address must be known by now

        let number_of_connected_peers = self.peer_book.read().number_of_connected_peers();
        let number_to_connect = self
            .environment
            .maximum_number_of_connected_peers()
            .saturating_sub(number_of_connected_peers);

        for peer_address in peers
            .iter()
            .take(number_to_connect as usize)
            .filter(|&peer_addr| *peer_addr != local_address)
            .copied()
        {
            // Inform the peer book that we found a peer.
            // The peer book will determine if we have seen the peer before,
            // and include the peer if it is new.
            self.peer_book.write().add_peer(peer_address);
        }
    }
}
