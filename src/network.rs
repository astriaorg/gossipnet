/// gossipnet implements a basic gossip network using libp2p.
/// It currently supports discovery via mdns and bootnodes, and eventually
/// will support DHT discovery.
use color_eyre::eyre::{eyre, Result, WrapErr};
use futures::StreamExt;
#[cfg(feature = "dht")]
use libp2p::kad::{
    record::{store::MemoryStore, Key},
    {Kademlia, KademliaConfig, KademliaEvent, QueryResult},
};
#[cfg(feature = "mdns")]
use libp2p::mdns;
use libp2p::{
    core::upgrade::Version,
    gossipsub::{self, Message, MessageId, TopicHash},
    identity,
    kad::GetProvidersOk,
    noise, ping,
    swarm::{NetworkBehaviour, Swarm, SwarmBuilder, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Transport,
};
use multiaddr::Protocol;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};
use tracing::{debug, info};

pub use libp2p::gossipsub::Sha256Topic;

#[derive(NetworkBehaviour)]
struct GossipnetBehaviour {
    ping: ping::Behaviour,
    gossipsub: gossipsub::Behaviour,
    #[cfg(feature = "mdns")]
    mdns: mdns::tokio::Behaviour,
    #[cfg(feature = "dht")]
    kademlia: Kademlia<MemoryStore>,
}

pub struct NetworkBuilder {
    bootnodes: Option<Vec<String>>,
    port: u16,
    // TODO: load key file or keypair
}

impl NetworkBuilder {
    pub fn new() -> Self {
        Self {
            bootnodes: None,
            port: 0, // random port
        }
    }

    pub fn bootnodes(mut self, bootnodes: Vec<String>) -> Self {
        self.bootnodes = Some(bootnodes);
        self
    }

    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn build(self) -> Result<Network> {
        Network::new(self.bootnodes, self.port)
    }
}

impl Default for NetworkBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Network {
    pub multiaddr: Multiaddr,
    swarm: Swarm<GossipnetBehaviour>,
    terminated: bool,
}

impl Network {
    pub fn new(bootnodes: Option<Vec<String>>, port: u16) -> Result<Self> {
        // TODO: store this on disk instead of randomly generating
        let local_key = identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());
        info!("local peer id: {local_peer_id:?}");

        let transport = tcp::tokio::Transport::default()
            .upgrade(Version::V1Lazy)
            .authenticate(noise::NoiseAuthenticated::xx(&local_key)?)
            .multiplex(yamux::YamuxConfig::default())
            .boxed();

        // content-address message by using the hash of it as an ID
        let message_id_fn = |message: &gossipsub::Message| {
            let mut s = DefaultHasher::new();
            message.data.hash(&mut s);
            gossipsub::MessageId::from(s.finish().to_string())
        };

        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(10))
            .validation_mode(gossipsub::ValidationMode::Strict) // the default is Strict (enforce message signing)
            .message_id_fn(message_id_fn) // content-address messages so that duplicates aren't propagated
            .build()
            .map_err(|e| eyre!("failed to build gossipsub config: {}", e))?;

        // build a gossipsub network behaviour
        let gossipsub = gossipsub::Behaviour::new(
            gossipsub::MessageAuthenticity::Signed(local_key),
            gossipsub_config,
        )
        .map_err(|e| eyre!("failed to create gossipsub behaviour: {}", e))?;

        let mut swarm = {
            #[cfg(feature = "dht")]
            let kademlia = {
                let mut cfg = KademliaConfig::default();
                cfg.set_query_timeout(Duration::from_secs(5 * 60));
                let store = MemoryStore::new(local_peer_id);
                Kademlia::with_config(local_peer_id, store, cfg)
            };

            #[cfg(feature = "mdns")]
            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;
            let behaviour = GossipnetBehaviour {
                gossipsub,
                #[cfg(feature = "mdns")]
                mdns,
                ping: ping::Behaviour::default(),
                #[cfg(feature = "dht")]
                kademlia,
            };
            SwarmBuilder::with_tokio_executor(transport, behaviour, local_peer_id).build()
        };

        let listen_addr = format!("/ip4/0.0.0.0/tcp/{}", port);
        swarm.listen_on(listen_addr.parse()?)?;

        if let Some(addrs) = bootnodes {
            addrs.iter().try_for_each(|addr| -> Result<_> {
                debug!("dialing {:?}", addr);
                let mut maddr: Multiaddr = addr.parse()?;
                swarm.dial(maddr.clone())?;
                debug!("dialed {addr}");

                let Some(peer_id) = maddr.pop() else {
                    return Err(eyre!("failed to parse peer id from addr: {}", addr));
                };

                match peer_id {
                    Protocol::P2p(peer_id) => {
                        let peer_id = match PeerId::from_multihash(peer_id) {
                            Ok(peer_id) => peer_id,
                            Err(e) => {
                                return Err(eyre!("failed to parse peer id from addr: {:?}", e));
                            }
                        };

                        swarm.behaviour_mut().kademlia.add_address(&peer_id, maddr);
                    }
                    _ => {
                        return Err(eyre!("failed to parse peer id from addr: {}", addr));
                    }
                }

                Ok(())
            })?;
        }

        let multiaddr = Multiaddr::from_str(&format!("{}/p2p/{}", listen_addr, local_peer_id))?;
        Ok(Network {
            multiaddr,
            swarm,
            terminated: false,
        })
    }

    #[cfg(feature = "dht")]
    pub async fn bootstrap(&mut self) -> Result<()> {
        self.swarm
            .behaviour_mut()
            .kademlia
            .bootstrap()
            .map(|_| ())
            .map_err(|e| eyre!(e))
    }

    #[cfg(feature = "dht")]
    pub async fn discover(&mut self, key: Key) {
        self.swarm.behaviour_mut().kademlia.get_providers(key);
    }

    pub async fn publish(&mut self, message: Vec<u8>, topic: Sha256Topic) -> Result<MessageId> {
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(topic, message)
            .wrap_err("failed to publish message")
    }

    pub fn subscribe(&mut self, topic: &Sha256Topic) {
        self.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(topic)
            .unwrap();
    }

    pub fn unsubscribe(&mut self, topic: &Sha256Topic) {
        self.swarm
            .behaviour_mut()
            .gossipsub
            .unsubscribe(topic)
            .unwrap();
    }
}

#[derive(Debug)]
pub enum Event {
    NewListenAddr(Multiaddr),
    Message(Message),
    #[cfg(feature = "mdns")]
    MdnsPeersConnected(Vec<PeerId>),
    #[cfg(feature = "mdns")]
    MdnsPeersDisconnected(Vec<PeerId>),
    PeerConnected(PeerId),
    PeerSubscribed(PeerId, TopicHash),
}

impl futures::Stream for Network {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(maybe_event) = self.swarm.poll_next_unpin(cx) {
            let Some(event) = maybe_event else {
                self.terminated = true;
                return Poll::Ready(None);
            };

            match event {
                #[cfg(feature = "dht")]
                SwarmEvent::Behaviour(GossipnetBehaviourEvent::Kademlia(
                    KademliaEvent::OutboundQueryProgressed { id, result, .. },
                )) => match result {
                    QueryResult::GetProviders(providers) => match providers {
                        Ok(providers) => {
                            match providers {
                                GetProvidersOk::FoundProviders { key, providers } => {
                                    debug!(
                                        "found {} providers for query id {:?}, key {:?}",
                                        providers.len(),
                                        id,
                                        key
                                    );
                                    for provider in providers {
                                        debug!("found provider {:?}", provider);
                                    }
                                }
                                GetProvidersOk::FinishedWithNoAdditionalRecord {
                                    closest_peers,
                                } => {
                                    debug!("finished with no additional record");
                                    for peer in closest_peers {
                                        debug!("closest peer {:?}", peer);
                                    }
                                }
                            };
                        }
                        Err(e) => {
                            debug!("failed to find providers for {:?}: {}", id, e);
                        }
                    },
                    QueryResult::Bootstrap(bootstrap) => {
                        if bootstrap.is_err() {
                            debug!("failed to bootstrap {:?}", id);
                            continue;
                        }

                        debug!("bootstrapping ok");
                    }
                    _ => {
                        debug!("query result for {:?}: {:?}", id, result);
                    }
                },
                #[cfg(feature = "mdns")]
                SwarmEvent::Behaviour(GossipnetBehaviourEvent::Mdns(mdns::Event::Discovered(
                    list,
                ))) => {
                    let peers = Vec::with_capacity(list.len());
                    for (peer_id, _multiaddr) in list {
                        debug!("mDNS discovered a new peer: {peer_id}");
                        self.swarm
                            .behaviour_mut()
                            .gossipsub
                            .add_explicit_peer(&peer_id);
                    }
                    return Poll::Ready(Some(Event::MdnsPeersConnected(peers)));
                }
                #[cfg(feature = "mdns")]
                SwarmEvent::Behaviour(GossipnetBehaviourEvent::Mdns(mdns::Event::Expired(
                    list,
                ))) => {
                    let peers = Vec::with_capacity(list.len());
                    for (peer_id, _multiaddr) in list {
                        debug!("mDNS discover peer has expired: {peer_id}");
                        self.swarm
                            .behaviour_mut()
                            .gossipsub
                            .remove_explicit_peer(&peer_id);
                    }
                    return Poll::Ready(Some(Event::MdnsPeersDisconnected(peers)));
                }
                SwarmEvent::Behaviour(GossipnetBehaviourEvent::Gossipsub(
                    gossipsub::Event::Message {
                        propagation_source: peer_id,
                        message_id: id,
                        message,
                    },
                )) => {
                    debug!(
                        "Got message: '{}' with id: {id} from peer: {peer_id}",
                        String::from_utf8_lossy(&message.data),
                    );
                    return Poll::Ready(Some(Event::Message(message)));
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    debug!("Local node is listening on {address}");
                    return Poll::Ready(Some(Event::NewListenAddr(address)));
                }
                SwarmEvent::Behaviour(GossipnetBehaviourEvent::Gossipsub(
                    gossipsub::Event::Subscribed { peer_id, topic },
                )) => {
                    debug!(
                        "Peer {peer_id} subscribed to topic: {topic:?}",
                        peer_id = peer_id,
                        topic = topic,
                    );
                    return Poll::Ready(Some(Event::PeerSubscribed(peer_id, topic)));
                }
                SwarmEvent::ConnectionEstablished {
                    peer_id,
                    endpoint: _,
                    num_established,
                    concurrent_dial_errors: _,
                    established_in: _,
                } => {
                    debug!(
                        "Connection with {peer_id} established (total: {num_established})",
                        peer_id = peer_id,
                        num_established = num_established,
                    );
                    self.swarm
                        .behaviour_mut()
                        .gossipsub
                        .add_explicit_peer(&peer_id);
                    return Poll::Ready(Some(Event::PeerConnected(peer_id)));
                }
                _ => {
                    debug!("unhandled swarm event: {:?}", event);
                }
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use futures::{channel::oneshot, join};
    use tokio::select;

    const TEST_TOPIC: &str = "test";

    #[tokio::test]
    async fn test_gossip_two_nodes() {
        let (bootnode_tx, bootnode_rx) = oneshot::channel();
        let (alice_tx, mut alice_rx) = oneshot::channel();

        let msg_a = b"hello world".to_vec();
        let recv_msg_a = msg_a.clone();
        let msg_b = b"i am responding".to_vec();
        let recv_msg_b = msg_b.clone();

        let alice_handle = tokio::task::spawn(async move {
            let topic = Sha256Topic::new(TEST_TOPIC);

            let mut alice = Network::new(None, 9000).unwrap();
            alice.subscribe(&topic);

            let Some(event) = alice.next().await else {
                panic!("expected stream event");
            };

            match event {
                Event::NewListenAddr(addr) => {
                    println!("Alice listening on {:?}", addr);
                    bootnode_tx.send(addr.clone()).unwrap();
                }
                _ => panic!("unexpected event"),
            };

            loop {
                let Some(event) = alice.next().await else {
                    break;
                };

                match event {
                    Event::PeerConnected(peer_id) => {
                        println!("Alice connected to {:?}", peer_id);
                    }
                    Event::PeerSubscribed(peer_id, topic_hash) => {
                        println!("Remote peer {:?} subscribed to {:?}", peer_id, topic_hash);
                        alice.publish(msg_a.clone(), topic.clone()).await.unwrap();
                    }
                    Event::Message(msg) => {
                        println!("Alice got message: {:?}", msg);
                        assert_eq!(msg.data, recv_msg_b);
                        alice_tx.send(()).unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        });

        let bob_handle = tokio::task::spawn(async move {
            let topic = Sha256Topic::new(TEST_TOPIC);

            let bootnode = bootnode_rx.await.unwrap();
            let mut bob = Network::new(Some(vec![bootnode.to_string()]), 9001).unwrap();
            bob.subscribe(&topic);

            loop {
                select! {
                    event = bob.next() => {
                        let Some(event) = event else {
                            continue;
                        };

                        match event {
                            Event::PeerConnected(peer_id) => {
                                println!("Bob connected to {:?}", peer_id);
                            }
                            Event::Message(msg) => {
                                println!("Bob got message: {:?}", msg);
                                assert_eq!(msg.data, recv_msg_a);
                                bob.publish(msg_b.clone(), topic.clone()).await.unwrap();
                            }
                            _ => {}
                        }
                    }
                    _ = &mut alice_rx => {
                        return;
                    }
                }
            }
        });

        let (res_a, res_b) = join!(alice_handle, bob_handle);
        res_a.unwrap();
        res_b.unwrap();
    }
}
