# P2P Network

Libp2p is a modular network designed for building p2p application. At its core, it provides transport protocols, peer discovery, security, and multiplexing so that developers can focus on application-level protocols.

## Transport and Security

-   Libp2p supports multiple underlying transport (TCP, QUIC, WebRTC). In this example code, `tcp::Config::default()` is responsible for creating a TCP transport.
    Combined with `noise::Config::default()` is used to provide authentication encryption. Multiplexed with `yamux::Config::default()`, which allows multiple streams to be sent over a single connection, improving efficiency.

-   Once two peers open a connection, they perform a cryptographic handshake: each side proves possession of its private key, exchange secure channel. From then on, messages are encrypted and integrity-protected.

```rust
.with_tcp(
    tcp::Config::default(),
    noise::Config::new,
    yamux::Config::default(),
)?

```

## Swarm and Network Behavior

-   A Swarm manages all active connections and drives every protocol behavior(NetworkBehavior). In our code, `ChatBehavior` derives from `NetworkBehavior` and bundles together a collection of sub-protocol implementations like `ping`, `identify`, `kademlia`, `request`, `response message`, `mdns`, `autoNAT`, `DUCTR` and `Gossipsub`
-   When you build a Swarm(`SwarmBuilder::with_new_identify()...build()`), Libp2p spin a background task to handle dialing new peers, listening for incoming connections, emitting events whenever something important happens (peers is discovered, message arrives, connection drops)

### 1. Identify

The identify behavior in libp2p allows two connected peers to exchange basic information about themselves immediately after establishing a connection. When a peer dials another peer, each side runs the identify protocol to share

1. Its peer ID and public key used for cryptographic authentication.
2. The list of protocol it supports
3. Its current listening address
4. Optional metadata such as agent version or observed version

By exchanging this data, each peer learns how to reach the others, what features the other supports, and can update routing tables or decide whether to use relay or direct connection. Identify also emits events whenever a peer's listening addresses change, allowing dynamic adaptation. In short, identify ensures that after a connection is made, both sides know each other's addressing and protocol capabilities, improving discovery, routing, and work resilience.

```rust
identify: identify::Behaviour,
identify: identify::Behaviour::new(identify::Config::new(
    PROTOCOL_NAME.to_string(),
    key_pair.public(),
)),

ChatBehaviorEvent::Identify(identify_event) => match identify_event {
    identify::Event::Received { connection_id, peer_id, info } => {
        // Update the Kademlia routing table with the peer's listen addresses
        println!("Received identify event from {:?} on connection {:?}: {:?}", peer_id, connection_id, info);

        // mark the peer as a relay if it supports the relay protocol
        let is_relay = info.protocols.iter().any(|p| *p == relay::HOP_PROTOCOL_NAME);

        for addr in info.listen_addrs {
            swarm.behaviour_mut().kademlia.add_address(&peer_id, addr.clone());
            if is_relay {
                let listen_addr = addr.clone().with_p2p(peer_id).unwrap().with(Protocol::P2pCircuit);
                println!("Try to listen on {:?}", listen_addr);
                swarm.listen_on(listen_addr)?;
            }
        }
    },

    identify::Event::Sent { connection_id, peer_id } => {
        println!("Sent identify to peer: {:?} on connection {:?}", peer_id, connection_id);
    },

    identify::Event::Error { connection_id, peer_id, error } => {
        eprintln!("Error in identify with peer {:?} on connection {:?}: {}", peer_id, connection_id, error);
    },

    identify::Event::Pushed {connection_id, peer_id, info } => {
        println!("Pushed identify event to {:?} on connection {:?}: {:?}", peer_id, connection_id, info);
    },
},

```

## Peer Discovery and Routing

### 1. mDNS (Multicast DNS)

On a local network, peers announce themselves by multicasting. When enabled `(mdns::tokio::Behaviour::new())`, your node periodically broadcasts heartbeat messages and listens for others doing the same. In our code, whenever a mDNS discovery event fires, your code learns a new PeerId and its address, then tell Kademlia and Gossipsub about the peer.

```rust
mdns::Event::Discovered(peers) => {
    for (peer_id, addr) in peers {
        println!("Discovered peer: {:?} at address: {:?}", peer_id, addr);
        swarm.add_peer_address(peer_id, addr.clone());
        swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
    }
},

```

### 2. Kademlia

Kademlia is a distributed hash table (DHT) protocol used for peer discovery and efficient routing in p2p networks. Each node has a unique identifier, and data or peers are also keyed by similar IDs. Kademlia use XOR metric on these IDs to measure "distance" organizing contacts into buckets based on the length of shared prefix. When a node wants to find a key(either to store or retrieve) , it performs an iterative lookup: it asked its closest nodes it knows (according to XOR distance) for nodes even closer to that key. This continues until no closer nodes can be found. As a result, any lookup completes in O(log n) steps, where n is the number of nodes. Kademlia's routing tables self-repair through periodic lookups and node pings, ensuring resilience even when many peers join or leave. In libp2p the `kad::Behavior` implements these mechanisms: it remains buckets of peer contracts, handle store/get request for records, and perform periodic refresh and bootstrap to keep DHT updated.

```rust
// declare identify behavior
kademlia: libp2p::kad::Behaviour<MemoryStore>,
kademlia: kad::Behaviour::with_config(
    key_pair.public().to_peer_id(),
    MemoryStore::new(key_pair.public().to_peer_id()),
    kad_config,
),

// handle kademlia events
ChatBehaviorEvent::Kademlia(kad_event) => match kad_event {
    kad::Event::InboundRequest { request } => {
        println!("Received Kademlia inbound request: {:?}", request);
    },
    kad::Event::OutboundQueryProgressed { id, result, stats, step } => {
        println!("Kademlia query progressed - ID: {:?}, Step: {:?}, Stats: {:?}", id, step, stats);
        match result {
            kad::QueryResult::Bootstrap(Ok(kad::BootstrapOk { peer, num_remaining })) => {
                println!("Bootstrap successful with peer {:?}, {} remaining", peer, num_remaining);
            },
            kad::QueryResult::Bootstrap(Err(e)) => {
                eprintln!("Bootstrap failed: {:?}", e);
            },
            _ => {
                println!("Query result: {:?}", result);
            }
        }
    },
    kad::Event::RoutingUpdated { peer, is_new_peer, addresses, bucket_range, old_peer } => {
        println!("Routing updated for 🟢 peer {:?}: is_new_peer: {}, addresses: {:?}, bucket_range: {:?}, ☑️ old_peer: {:?}",
            peer, is_new_peer, addresses, bucket_range, old_peer);
        addresses.iter().for_each(|addr| {
            if let Err(error) = swarm.dial(addr.clone()) {
                eprintln!("Failed to dial address {:?} for peer {:?}: {}", addr, peer, error);
            }
        });
    },
    kad::Event::UnroutablePeer { peer } => {
        println!("Peer {:?} is unroutable", peer);
    },
    kad::Event::RoutablePeer { peer, address } => {
        println!("Peer {:?} is routable at address {:?}", peer, address);
        swarm.behaviour_mut().kademlia.add_address(&peer, address);
    },
    kad::Event::PendingRoutablePeer { peer, address } => {
        println!("Peer {:?} is pending routable at address {:?}", peer, address);
    },
    kad::Event::ModeChanged { new_mode } => {
        println!("Kademlia mode changed to: {:?}", new_mode);
    },
},

```

## Address Management and Identify

-   When two peers connect, the identify protocol immediately exchange each peer's listening addresses, supported protocols and public key, each node updates its peerstore(a local database of who's reachable where). In our code, when `identify::Event::Received` arrives, call `kademlia.add_address(&peer_id, addr.clone())` and `gossipsub.add_explicit_peer(&peer_id)`. If that peer supports the relay protocol, you automatically start listening on `/p2p-circuit` address routed through that relay.

## NAT Traversal

### 1. AutoNAT

AutoNAT is libp2p behavior that automatically detect whether a node is behind a NAT/firewall or publicly reachable. A node running AutoNAT will periodically as like a client and send DialRequest message to designated AutoNAT server(or other participating peers). Each server upon receiving a DialRequest, attempts to dial back to the client's multi address. If the dial-back is successful, the client knows that it is publicly reachable; if it fails, the client infers it is behind a restrictive NAT/Firewall. AutoNAT then provides real-time reachability status, allowing node to decide when to use `hole-punch` or `relay service` (dcutr) to establish connection. It also let other peers query its reachability through standard protocols, improving overall connection reliability in the network.

```rust

autonat: autonat::Behaviour,

autonat: autonat::Behaviour::new(
    key_pair.public().to_peer_id(),
    autonat::Config::default(),
),

ChatBehaviorEvent::Autonat(autonat_event) => match autonat_event {
    autonat::Event::InboundProbe(event) => {
        println!("Received inbound probe: {:?}", event);
    },
    autonat::Event::OutboundProbe(event) => {
        println!("Received outbound probe: {:?}", event);
    },
    autonat::Event::StatusChanged { old, new } => {
        println!("Status changed from {:?} to {:?}", old, new);
    },

},

```

### 2. DCUTR(Direct Connection Upgrade Through Relay)

`DCUTR` is a module in libp2p that helps two peers establish a direct connection when both are behind NATs or firewalls. When a peer discovers it cannot dial another peer directly - often detected via AutoNAT - it uses a relay as a rendezvous each peer sends a "hole-punch" request through the relay. The relay forward these requests so than each peer learns the other's extra address and port.

Once both peers know each other's external endpoint, They attempt simultaneous connection to traverse NAT mappings. If the hole-punch is successful, the peers can upgrade from the relay path to a direct connection.

In short, `dcutr` leverages a relay just long enough to exchange addressing information and coordinate NAT traversal, then switches to peer-to-peer for better performance.

```rust
dcutr: dcutr::Behaviour,
dcutr: dcutr::Behaviour::new(
    key_pair.public().to_peer_id(),
    ),

```

## Relay

-   Libp2p can act as a `relay server`, forwarding traffic for nodes that cannot talk to each other directly. In `ChatBehavior`, `relay::Behaviour` creates a relay service if you want to allow other peers to hop through you. Meanwhile, `relay::client::Behaviour` lets you code connect to other relay servers so that you can reach peers behind NATs.
-

## Message Protocols

-   Request/Response. This implements a simple `ask-and-reply` patterns over a custom protocol `p2p-chat-v1/1.0.0`. When you call `messaging.send_request(&peer_id, MessageRequest{...})`, Libp2p opens a substream over encrypted, multiplexed connection, send your serialized request and awaits a response. When the remote receives that request, it triggers `ChatBehaviorEvent::Messaging(Event::Message::Request)`, so that you can reply with `send_request(channel, MessageResponse{...})`

-   Gossipsub. A publish/subscribe system built on the top of the DHT(or explicit_peer_list). In code, `gossipsub::Behavior::new(MessageAuthenticity::Signed(key_pair.clone(), config))` create a Gossipsub route than cryptographically signs each message. You then call `subscribe(&Sha256Topic::new("chat"))` to join the “chat” topic. When you publish with `gossipsub.publish(topic, data)`, your message is relayed through the network in a gossip fashion-every peer forwards it peers they known in that topic. eventually reaching all peers they know in the topic.
