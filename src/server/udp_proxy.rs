//! sing UDP-over-TCP forwarding for AnyTLS streams.
//!
//! Version 2 begins with `isConnect` followed by a SOCKS address (types 1/3/4).
//! Connected packets contain a big-endian u16 length followed by the payload.
//! Unconnected packets additionally carry a destination before the length, using
//! UoT address types 0/1/2 for IPv4/IPv6/domain. Responses carry the UDP source.
//! Version 1 uses the same unconnected packet format without an initial request.

use crate::session::{Stream, StreamReader};
use crate::util::{AnyTlsError, Result, resolve_host_with_cache};
use bytes::{BufMut, Bytes, BytesMut};
use std::future::pending;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, OnceCell};
use tracing::{Instrument, field, info_span};

const MAX_UDP_PACKET_SIZE: usize = u16::MAX as usize;

/// Handle a version 2 stream addressed to `sp.v2.udp-over-tcp.arpa`.
pub async fn handle_udp_over_tcp(stream: Arc<Stream>) -> Result<()> {
    proxy(stream, true).await
}

/// Handle a legacy version 1 stream addressed to `sp.udp-over-tcp.arpa`.
pub async fn handle_udp_over_tcp_v1(stream: Arc<Stream>) -> Result<()> {
    proxy(stream, false).await
}

async fn proxy(stream: Arc<Stream>, version_two: bool) -> Result<()> {
    let span = info_span!(
        "anytls.udp.proxy",
        stream_id = stream.id(),
        version = if version_two { 2 } else { 1 },
        target = field::Empty,
        packets_in = field::Empty,
        packets_out = field::Empty,
        bytes_in = field::Empty,
        bytes_out = field::Empty
    );
    let stats = UdpStats::default();
    let result = async {
        let target = if version_two {
            let mut reader = stream.reader().lock().await;
            read_initial_request(&mut reader).await?
        } else {
            None
        };
        let sockets = UdpSockets::default();
        if let Some(target) = target {
            span.record("target", field::display(target));
            // Connecting also makes the kernel reject packets from other peers.
            sockets.socket_for(target).await?.connect(target).await?;
        }

        tokio::select! {
            result = stream_to_udp(&stream, &sockets, target, &stats) => result,
            result = udp_to_stream(&stream, &sockets, target.is_some(), &stats) => result,
        }
    }
    .instrument(span.clone())
    .await;

    span.record("packets_out", stats.packets_out.load(Ordering::Relaxed));
    span.record("bytes_out", stats.bytes_out.load(Ordering::Relaxed));
    span.record("packets_in", stats.packets_in.load(Ordering::Relaxed));
    span.record("bytes_in", stats.bytes_in.load(Ordering::Relaxed));
    result
}

#[derive(Default)]
struct UdpStats {
    packets_out: AtomicU64,
    bytes_out: AtomicU64,
    packets_in: AtomicU64,
    bytes_in: AtomicU64,
}

/// A packet association may address both IP families. Bind each family only
/// when used, so IPv4 traffic also works on hosts with IPv6 disabled (and vice
/// versa), without depending on platform-specific dual-stack socket defaults.
#[derive(Default)]
struct UdpSockets {
    ipv4: OnceCell<UdpSocket>,
    ipv6: OnceCell<UdpSocket>,
    initialized: Notify,
}

impl UdpSockets {
    async fn socket_for(&self, target: SocketAddr) -> io::Result<&UdpSocket> {
        let (cell, bind_addr) = match target {
            SocketAddr::V4(_) => (&self.ipv4, SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))),
            SocketAddr::V6(_) => (&self.ipv6, SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))),
        };
        if let Some(socket) = cell.get() {
            return Ok(socket);
        }
        let socket = cell.get_or_try_init(|| UdpSocket::bind(bind_addr)).await?;
        // Notify after publishing the socket, otherwise the receiver could wake
        // before OnceCell contains it and miss the newly available family.
        self.initialized.notify_one();
        Ok(socket)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            let initialized = self.initialized.notified();
            let ipv4 = self.ipv4.get();
            let ipv6 = self.ipv6.get();
            let socket = tokio::select! {
                _ = initialized => continue,
                result = readable(ipv4) => {
                    result?;
                    ipv4.expect("readable requires an initialized socket")
                }
                result = readable(ipv6) => {
                    result?;
                    ipv6.expect("readable requires an initialized socket")
                }
            };
            match socket.try_recv_from(buf) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                result => return result,
            }
        }
    }
}

async fn readable(socket: Option<&UdpSocket>) -> io::Result<()> {
    match socket {
        Some(socket) => socket.readable().await,
        None => pending().await,
    }
}

enum Host {
    Ip(IpAddr),
    Domain(String),
}

struct Destination {
    host: Host,
    port: u16,
}

impl Destination {
    async fn resolve(self) -> Result<SocketAddr> {
        match self.host {
            Host::Ip(ip) => Ok(SocketAddr::new(ip, self.port)),
            Host::Domain(host) => resolve_host_with_cache(&host, self.port).await,
        }
    }
}

/// `None` denotes unconnected mode. Its initial destination is a hint, not the
/// destination of every packet, and must not cause a DNS lookup or UDP connect.
async fn read_initial_request(reader: &mut StreamReader) -> Result<Option<SocketAddr>> {
    let mode = read_byte(reader).await?;
    if mode > 1 {
        return Err(AnyTlsError::Protocol(format!(
            "Invalid UDP-over-TCP isConnect value: {mode}"
        )));
    }
    let family = read_byte(reader).await?;
    let destination = read_destination(reader, family, true).await?;
    if mode == 1 {
        Ok(Some(destination.resolve().await?))
    } else {
        Ok(None)
    }
}

async fn read_byte(reader: &mut StreamReader) -> Result<u8> {
    let mut byte = [0];
    reader.read_exact(&mut byte).await?;
    Ok(byte[0])
}

async fn read_destination(
    reader: &mut StreamReader,
    family: u8,
    socks_format: bool,
) -> Result<Destination> {
    // Initial requests use SOCKS ATYP; packet headers use sing's UoT parser.
    let (ipv4, ipv6, domain) = if socks_format { (1, 4, 3) } else { (0, 1, 2) };
    let host = if family == ipv4 {
        let mut address = [0; 4];
        reader.read_exact(&mut address).await?;
        Host::Ip(IpAddr::V4(Ipv4Addr::from(address)))
    } else if family == ipv6 {
        let mut address = [0; 16];
        reader.read_exact(&mut address).await?;
        let address = Ipv6Addr::from(address);
        Host::Ip(match address.to_ipv4_mapped() {
            Some(ipv4) => IpAddr::V4(ipv4),
            None => IpAddr::V6(address),
        })
    } else if family == domain {
        let length = read_byte(reader).await? as usize;
        if length == 0 {
            return Err(AnyTlsError::Protocol("Empty UDP destination domain".into()));
        }
        let mut name = vec![0; length];
        reader.read_exact(&mut name).await?;
        Host::Domain(
            String::from_utf8(name)
                .map_err(|error| AnyTlsError::Protocol(format!("Invalid UDP domain: {error}")))?,
        )
    } else {
        return Err(AnyTlsError::Protocol(format!(
            "Unknown UDP address type: {family}"
        )));
    };
    let mut port = [0; 2];
    reader.read_exact(&mut port).await?;
    Ok(Destination {
        host,
        port: u16::from_be_bytes(port),
    })
}

async fn stream_to_udp(
    stream: &Stream,
    sockets: &UdpSockets,
    connected_target: Option<SocketAddr>,
    stats: &UdpStats,
) -> Result<()> {
    let mut reader = stream.reader().lock().await;
    while let Some((target, payload)) = read_udp_packet(&mut reader, connected_target).await? {
        let socket = sockets.socket_for(target).await?;
        let sent = if connected_target.is_some() {
            socket.send(&payload).await?
        } else {
            socket.send_to(&payload, target).await?
        };
        if sent != payload.len() {
            return Err(
                io::Error::new(io::ErrorKind::WriteZero, "Incomplete UDP datagram send").into(),
            );
        }
        stats.packets_out.fetch_add(1, Ordering::Relaxed);
        stats.bytes_out.fetch_add(sent as u64, Ordering::Relaxed);
    }
    Ok(())
}

async fn udp_to_stream(
    stream: &Stream,
    sockets: &UdpSockets,
    is_connect: bool,
    stats: &UdpStats,
) -> Result<()> {
    let mut buf = vec![0; MAX_UDP_PACKET_SIZE];
    loop {
        let (length, source) = sockets.recv_from(&mut buf).await?;
        let packet = encode_udp_packet(&buf[..length], (!is_connect).then_some(source))?;
        stream
            .send_data(packet)
            .await
            .map_err(|_| AnyTlsError::SessionClosed)?;
        stats.packets_in.fetch_add(1, Ordering::Relaxed);
        stats.bytes_in.fetch_add(length as u64, Ordering::Relaxed);
    }
}

/// EOF is clean only before a new packet begins. A zero length is a valid UDP
/// datagram; EOF within an address, length, or payload is a truncated packet.
async fn read_udp_packet(
    reader: &mut StreamReader,
    connected_target: Option<SocketAddr>,
) -> Result<Option<(SocketAddr, Vec<u8>)>> {
    let mut first = [0];
    if reader.read(&mut first).await? == 0 {
        return Ok(None);
    }
    let mut length = [0; 2];
    let target = if let Some(target) = connected_target {
        length[0] = first[0];
        reader.read_exact(&mut length[1..]).await?;
        target
    } else {
        let destination = read_destination(reader, first[0], false).await?;
        reader.read_exact(&mut length).await?;
        destination.resolve().await?
    };
    let mut payload = vec![0; u16::from_be_bytes(length) as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Some((target, payload)))
}

fn encode_udp_packet(payload: &[u8], source: Option<SocketAddr>) -> Result<Bytes> {
    if payload.len() > MAX_UDP_PACKET_SIZE {
        return Err(AnyTlsError::Protocol(format!(
            "UDP packet too large: {} bytes",
            payload.len()
        )));
    }
    let mut packet = BytesMut::with_capacity(19 + 2 + payload.len());
    if let Some(source) = source {
        match source {
            SocketAddr::V4(address) => {
                packet.put_u8(0);
                packet.put_slice(&address.ip().octets());
            }
            SocketAddr::V6(address) => {
                packet.put_u8(1);
                packet.put_slice(&address.ip().octets());
            }
        }
        packet.put_u16(source.port());
    }
    packet.put_u16(payload.len() as u16);
    packet.put_slice(payload);
    Ok(packet.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_empty_packet() {
        assert_eq!(&encode_udp_packet(b"", None).unwrap()[..], &[0, 0]);
    }

    #[test]
    fn uot_length_is_independent_of_anytls_frame_limit() {
        let payload = vec![42; MAX_UDP_PACKET_SIZE];
        for source in [None, Some("[::1]:53".parse().unwrap())] {
            let encoded = encode_udp_packet(&payload, source).unwrap();
            let length_offset = if source.is_some() { 19 } else { 0 };
            assert_eq!(&encoded[length_offset..length_offset + 2], &[255, 255]);
            assert_eq!(&encoded[length_offset + 2..], &payload);
            // The session writer fragments the encoded stream into PSH frames.
            assert!(encoded.len() > u16::MAX as usize);
        }
        assert!(encode_udp_packet(&vec![0; MAX_UDP_PACKET_SIZE + 1], None).is_err());
    }
}
