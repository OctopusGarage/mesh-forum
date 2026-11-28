//! Application-scoped signed LAN discovery over the generic SDK transport.

use super::Invite;
use mesh_connect::{
    discovery::announce::{Announce, decode, encode},
    net::{DISCOVERY_MULTICAST_GROUP, discovery_socket, send_discovery_multicast},
};
use mesh_identity::device::DeviceIdentity;
use std::{
    collections::BTreeMap,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::RwLock, task::JoinHandle, time::interval};

pub const LAN_DISCOVERY_PORT: u16 = 47625;
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(3);
const PRESENCE_TTL: Duration = Duration::from_secs(12);
const MAX_CANDIDATES: usize = 64;
const MAX_DATAGRAM: usize = 1200;

pub(super) type Candidates = Arc<RwLock<BTreeMap<[u8; 32], (Invite, Instant)>>>;

pub(super) async fn active(roster: &Candidates) -> Vec<Invite> {
    roster
        .read()
        .await
        .values()
        .filter_map(|(invite, seen)| (seen.elapsed() < PRESENCE_TTL).then_some(*invite))
        .collect()
}

fn discovery_namespace() -> [u8; 32] {
    *b"mesh-forum-discovery-v1........."
}

fn scope(board: [u8; 32]) -> String {
    let hex: String = board.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("mesh-forum/1/{hex}")
}

fn candidate(packet: &[u8], source: IpAddr, expected: &str, own: [u8; 32]) -> Option<Invite> {
    if packet.len() > MAX_DATAGRAM || source.is_unspecified() || source.is_multicast() {
        return None;
    }
    let announce = decode(packet)?;
    if announce.namespace != discovery_namespace()
        || announce.name != expected
        || announce.x25519_pub == own
        || !announce.valid_presence()
    {
        return None;
    }
    Some(Invite {
        address: SocketAddr::new(source, announce.tcp_port),
        public_key: announce.x25519_pub,
    })
}

pub(super) fn start(
    identity: &DeviceIdentity,
    board: [u8; 32],
    tcp_port: u16,
    udp_port: u16,
    roster: Candidates,
) -> io::Result<JoinHandle<()>> {
    let socket = discovery_socket(udp_port)?;
    let expected = scope(board);
    let own = identity.public().x25519_pub;
    let announce = encode(
        &Announce::for_protocol(
            identity,
            discovery_namespace(),
            expected.clone(),
            tcp_port,
            vec![],
            vec![],
        )
        .expect("bounded forum discovery"),
    )
    .map_err(io::Error::other)?;
    let multicast = SocketAddr::from((DISCOVERY_MULTICAST_GROUP, udp_port));
    let broadcast = SocketAddr::from((Ipv4Addr::BROADCAST, udp_port));
    Ok(tokio::spawn(async move {
        let mut timer = interval(ANNOUNCE_INTERVAL);
        let mut packet = [0; MAX_DATAGRAM + 1];
        loop {
            tokio::select! {
                _ = timer.tick() => {
                    let _ = send_discovery_multicast(&announce, multicast).await;
                    let _ = socket.send_to(&announce, broadcast).await;
                    roster.write().await.retain(|_, (_, seen)| seen.elapsed() < PRESENCE_TTL);
                }
                result = socket.recv_from(&mut packet) => {
                    let Ok((size, source)) = result else { break };
                    let Some(invite) = candidate(&packet[..size], source.ip(), &expected, own) else { continue };
                    let mut roster = roster.write().await;
                    if roster.len() < MAX_CANDIDATES || roster.contains_key(&invite.public_key) {
                        roster.insert(invite.public_key, (invite, Instant::now()));
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_discovery_scope_signature_and_uses_observed_source() {
        let identity = DeviceIdentity::generate();
        let name = scope([3; 32]);
        let announce = Announce::for_protocol(
            &identity,
            discovery_namespace(),
            name.clone(),
            7444,
            vec![],
            vec![],
        )
        .unwrap();
        let source = "192.0.2.15".parse().unwrap();
        let invite = candidate(&encode(&announce).unwrap(), source, &name, [0; 32]).unwrap();
        assert_eq!(invite.address, "192.0.2.15:7444".parse().unwrap());
        assert!(
            candidate(
                &encode(&announce).unwrap(),
                source,
                &scope([4; 32]),
                [0; 32]
            )
            .is_none()
        );
        let mut tampered = announce;
        tampered.tcp_port = 7445;
        assert!(candidate(&encode(&tampered).unwrap(), source, &name, [0; 32]).is_none());
    }
}
