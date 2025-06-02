use std::{
    env,
    hash::{DefaultHasher, Hash, Hasher},
    time::{Duration, UNIX_EPOCH},
};

use libp2p::{
    autonat, dcutr,
    futures::{select, StreamExt},
    gossipsub::{self, MessageAuthenticity, Sha256Topic},
    identify,
    kad::{self, store::MemoryStore, Mode},
    mdns,
    multiaddr::Protocol,
    noise, ping, relay,
    request_response::{self, json},
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, StreamProtocol,
};

use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncBufReadExt, BufReader};
use tokio_stream::wrappers::LinesStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageRequest {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageResponse {
    pub ack: bool,
}

#[derive(NetworkBehaviour)] // poll behaviour for event, handle event router, protocol registration,
struct ChatBehavior {
    ping: ping::Behaviour,
    messaging: json::Behaviour<MessageRequest, MessageResponse>,
    mdns: Toggle<mdns::tokio::Behaviour>,
    identify: identify::Behaviour,
    kademlia: libp2p::kad::Behaviour<MemoryStore>,
    autonat: autonat::Behaviour,
    relay_server: relay::Behaviour,
    relay_client: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    gossipsub: gossipsub::Behaviour,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // declare the port
    // let port = std::env::var("CHAT_P2P_PORT")
    //     .unwrap_or_else(|_| "9999".into())
    //     .parse::<u16>()?;
    // declare the peer address
    // multi address is a self-describing network address format used in libp2p
    //ipv4 + tcp + specific peer id = "/ip4/127.0.0.1/tcp/9999/p2p/12D3KooWBmwkafWE2fqfzS96VoTZMJSSo8RcXzSBrcwbtGjWqodT"
    // let peer: Multiaddr = std::env::var("CHAT_PEER")?.parse()?;
    const PROTOCOL_NAME: &str = "/p2p-chat-v1/1.0.0";
    const KADEMLIA_PROTOCOL: &str = "/p2p-chat-v1/kad/1.0.0";

    const TOPIC: &str = "chat";

    let boost_peers = env::var("CHAT_BOOTSTRAP_PEERS")
        .unwrap_or_else(|_| String::new())
        .split(',')
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    println!("Bootstrap peers: {:?}", boost_peers);

    let mdns_enabled = std::env::var("CHAT_MDNS_ENABLED")
        .map(|v| v.parse::<bool>().unwrap_or(false))
        .unwrap_or(false);

    let mut kad_config = kad::Config::new(StreamProtocol::new(KADEMLIA_PROTOCOL));
    kad_config.set_periodic_bootstrap_interval(Some(Duration::from_secs(10)));

    // gossipsub
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10))
        .validation_mode(gossipsub::ValidationMode::Strict)
        .message_id_fn(|msg: &gossipsub::Message| {
            // Use the message's data + topic + peer_id + timestamp to create a unique message ID
            let mut hasher = DefaultHasher::new();
            msg.data.hash(&mut hasher);
            msg.topic.hash(&mut hasher);
            if let Some(peer_id) = msg.source {
                peer_id.hash(&mut hasher);
            }

            let now = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis();
            now.hash(&mut hasher);
            gossipsub::MessageId::from(hasher.finish().to_string())
        })
        .build()?;

    // manage connections, handle protocols, routes events, coordinate behaviors
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key_pair, relay_client| ChatBehavior {
            ping: ping::Behaviour::new(
                ping::Config::new().with_interval(std::time::Duration::from_secs(10)),
            ),
            messaging: json::Behaviour::new(
                [(
                    StreamProtocol::new(PROTOCOL_NAME),
                    request_response::ProtocolSupport::Full,
                )],
                request_response::Config::default(),
            ),
            mdns: if mdns_enabled {
                Toggle::from(Some(
                    mdns::tokio::Behaviour::new(
                        mdns::Config::default(),
                        key_pair.public().to_peer_id(),
                    )
                    .unwrap(),
                ))
            } else {
                Toggle::from(None)
            },
            identify: identify::Behaviour::new(identify::Config::new(
                PROTOCOL_NAME.to_string(),
                key_pair.public(),
            )),
            kademlia: kad::Behaviour::with_config(
                key_pair.public().to_peer_id(),
                MemoryStore::new(key_pair.public().to_peer_id()),
                kad_config,
            ),
            autonat: autonat::Behaviour::new(
                key_pair.public().to_peer_id(),
                autonat::Config::default(),
            ),
            relay_server: relay::Behaviour::new(
                key_pair.public().to_peer_id(),
                relay::Config::default(),
            ),
            relay_client: relay_client,

            dcutr: dcutr::Behaviour::new(key_pair.public().to_peer_id()),

            gossipsub: gossipsub::Behaviour::new(
                MessageAuthenticity::Signed(key_pair.clone()),
                gossipsub_config,
            )
            .unwrap(),
        })?
        .with_swarm_config(|conf| {
            conf.with_idle_connection_timeout(core::time::Duration::from_secs(30))
            // dial config
        })
        .build();

    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    // set server mode for kademlia
    swarm.behaviour_mut().kademlia.set_mode(Some(Mode::Server));

    // dial the peers
    // swarm.dial(peer.clone())?;

    println!("PeerID: {:?}", swarm.local_peer_id());

    for boost_peer in boost_peers {
        if boost_peer.is_empty() {
            continue; // skip empty bootstrap peers
        }
        let addr: Multiaddr = boost_peer.parse()?;
        let peer_id = addr
            .iter()
            .map(|_addr_str| {
                if let Some(Protocol::P2p(peer_id)) = addr.iter().last() {
                    return Some(peer_id);
                }
                None
            })
            .filter(Option::is_some)
            .last()
            .ok_or(anyhow::anyhow!(
                "Invalid bootstrap peer address: {}",
                boost_peer
            ))?
            .ok_or(anyhow::anyhow!("Bootstrap peer address: {}", boost_peer))?;
        swarm
            .behaviour_mut()
            .kademlia
            .add_address(&peer_id, addr.clone());
        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
    }

    // subscribe the gossip topic
    swarm
        .behaviour_mut()
        .gossipsub
        .subscribe(&Sha256Topic::new(TOPIC))?;

    let mut stdin = LinesStream::new(BufReader::new(io::stdin()).lines()).fuse();

    loop {
        select! {
            // process events from the swarm
            event = swarm.select_next_some() => match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Listening on: {}", address);
                },

                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    println!("Connected to peer: {:?}", peer_id);
                },

                // process behaviour events
                SwarmEvent::Behaviour(event) => match event {
                    ChatBehaviorEvent::Ping(ping_event) => {
                        println!("Received Pong from {:?}", ping_event);
                    },

                    ChatBehaviorEvent::Messaging(message_event) => match message_event {
                        request_response::Event::Message{peer, connection_id, message} => match message{
                            request_response::Message::Request{request_id, request, channel} => {
                                println!("Received message from request {:?} which peer is {:?} on connection {:?}: {:?}",request_id, peer, connection_id, request);
                                swarm.behaviour_mut().messaging.send_response(channel, MessageResponse { ack: true })
                                    .expect("Failed to send response");
                            },

                            request_response::Message::Response{request_id, response} => {
                                println!("Received response from request is {:?} which peer is {:?} on connection {:?}: {:?}", request_id, peer, connection_id, response);
                            }
                        },
                            request_response::Event::OutboundFailure{peer, connection_id, request_id, error} => {
                                eprintln!("¨❌Failed to send message to peer {:?} on connection {:?} with request ID {:?}: {}", peer, connection_id, request_id, error);
                        },
                        request_response::Event::InboundFailure{peer, connection_id, request_id, error} => {
                            eprintln!("¨❌Failed to receive message from peer {:?} on connection {:?} with request ID {:?}: {}", peer, connection_id, request_id, error);
                        },
                        request_response::Event::ResponseSent{..} => {},
                    },

                    // process mDNS events
                    ChatBehaviorEvent::Mdns(mdns_event) => match mdns_event {
                        mdns::Event::Discovered(peers) => {
                            for (peer_id, addr) in peers {
                                println!("Discovered peer: {:?} at address: {:?}", peer_id, addr);
                                swarm.add_peer_address(peer_id, addr.clone());
                                swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
                                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                            }
                        },
                        mdns::Event::Expired(peers) => {
                            for (peer_id, _) in peers {
                                println!("Expired peer: {:?}", peer_id);
                            }
                        },
                    },
                    ChatBehaviorEvent::Identify(identify_event) => match identify_event {
                        identify::Event::Received { connection_id, peer_id, info } => {
                            // Update the Kademlia routing table with the peer's listen addresses
                            println!("Received identify event from {:?} on connection {:?}: {:?}", peer_id, connection_id, info);

                            // mark the peer as a relay if it supports the relay protocol
                            let is_relay = info.protocols.iter().any(|p| *p == relay::HOP_PROTOCOL_NAME);

                            for addr in info.listen_addrs {
                                swarm.behaviour_mut().kademlia.add_address(&peer_id, addr.clone());
                                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
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
                            // addresses.iter().for_each(|addr| {
                            //     if let Err(error) = swarm.dial(addr.clone()) {
                            //         eprintln!("Failed to dial address {:?} for peer {:?}: {}", addr, peer, error);
                            //     }
                            // });
                            swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);
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

                    ChatBehaviorEvent::RelayServer(relay_event) => {
                        println!("Relay server event: {:?}", relay_event);
                    },

                    ChatBehaviorEvent::RelayClient(relay_event) => {
                        println!("Relay client event: {:?}", relay_event);
                    },

                    ChatBehaviorEvent::Dcutr(dcutr_event) => {
                        println!("DCUTR event: {:?}", dcutr_event);
                    },

                    ChatBehaviorEvent::Gossipsub(gossipsub_event) => match gossipsub_event {
                        gossipsub::Event::Message { message, propagation_source, .. } => {
                            println!("Received Gossipsub message from {:?}: {:?}", propagation_source, message);
                        },
                        gossipsub::Event::Subscribed { peer_id, topic } => {
                            println!("Peer {:?} subscribed to topic {:?}", peer_id, topic);
                        },
                        gossipsub::Event::Unsubscribed { peer_id, topic } => {
                            println!("Peer {:?} unsubscribed from topic {:?}", peer_id, topic);
                        },
                        gossipsub::Event::GossipsubNotSupported { peer_id } => {
                            println!("Peer {:?} does not support Gossipsub", peer_id);
                        },
                        gossipsub::Event::SlowPeer { peer_id, failed_messages } => {
                            println!("Peer {:?} slow to respond: {:?}", peer_id, failed_messages);
                        },
                    },

                },
                _ => {}
            },

            // read from stdin then send it to request
            maybe_line = stdin.next() => {
                match maybe_line {
                    Some(Ok(line)) => {
                        // let connected_peers: Vec<PeerId> = swarm.connected_peers().cloned().collect();
                        // for peer_id in connected_peers {
                        //     let request = MessageRequest { message: line.clone() };
                        //     println!("Sending message to {:?}: {}", peer_id, request.message);
                        //     let _request_id = swarm.behaviour_mut().messaging.send_request(&peer_id, request);
                        // }
                        let topic = Sha256Topic::new(TOPIC);
                        match swarm.behaviour_mut().gossipsub.publish(topic, line.as_bytes()) {
                            Ok(_) => println!("Published message to peer_id: {:?}", swarm.local_peer_id()),
                            Err(e) => eprintln!("Failed to publish message to topic {:?}: {}", TOPIC, e),
                        }
                    }
                    Some(Err(e)) => eprintln!("Error reading from stdin: {}", e),
                    None => break, // EOF
                }
            }

        }
    }
    Ok(())
}
