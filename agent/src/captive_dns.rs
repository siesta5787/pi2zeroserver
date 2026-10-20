//! DNS for the setup hotspot: every name is the box. Phones check a known web
//! address when they join a network; landing on the box instead makes them
//! offer to open its page (the "sign in to network" prompt).
//!
//! Listens only on the hotspot address, which exists only while the hotspot is
//! on, so the home network never sees it.

use std::net::Ipv4Addr;
use std::time::Duration;

/// The reply to a DNS query: an A record with `addr` for A queries, no
/// answers for other types. None for anything that isn't a plain query.
pub fn answer(query: &[u8], addr: Ipv4Addr) -> Option<Vec<u8>> {
    let header = query.get(..12)?;
    let flags = u16::from_be_bytes([header[2], header[3]]);
    if flags & 0x8000 != 0 {
        return None; // a response, not a query
    }
    let opcode = (flags >> 11) & 0xf;
    let questions = u16::from_be_bytes([header[4], header[5]]);
    let mut reply = Vec::with_capacity(query.len() + 16);
    reply.extend_from_slice(&header[..2]);
    // Response, authoritative, recursion desired copied from the query
    let reply_flags = 0x8000 | (opcode << 11) | 0x0400 | (flags & 0x0100);
    if opcode != 0 || questions != 1 {
        reply.extend_from_slice(&(reply_flags | 4).to_be_bytes()); // NOTIMP
        reply.extend_from_slice(&[0; 8]);
        return Some(reply);
    }
    // The question: labels up to a zero byte, then type and class
    let mut end = 12;
    loop {
        let len = *query.get(end)? as usize;
        end += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 {
            return None; // no compression in questions
        }
        end += len;
    }
    let tail = query.get(end..end + 4)?;
    let qtype = u16::from_be_bytes([tail[0], tail[1]]);
    let qclass = u16::from_be_bytes([tail[2], tail[3]]);
    end += 4;
    let a_record = qtype == 1 && qclass == 1;
    reply.extend_from_slice(&reply_flags.to_be_bytes());
    reply.extend_from_slice(&[0, 1, 0, a_record as u8, 0, 0, 0, 0]);
    reply.extend_from_slice(&query[12..end]);
    if a_record {
        // Name: pointer to the question; type A, class IN, TTL 10 s, 4 bytes
        reply.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 10, 0, 4]);
        reply.extend_from_slice(&addr.octets());
    }
    Some(reply)
}

/// A UDP socket on `addr`:`port` that may be bound before the address exists
/// (IP_FREEBIND): the hotspot address only appears once the hotspot is up
fn bind_early(addr: Ipv4Addr, port: u16) -> std::io::Result<std::net::UdpSocket> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let check = |rc: libc::c_int| if rc < 0 { Err(std::io::Error::last_os_error()) } else { Ok(rc) };
    // SAFETY: plain socket calls on a descriptor owned by `socket` from here on
    unsafe {
        let fd = check(libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        ))?;
        let socket = std::net::UdpSocket::from_raw_fd(fd);
        let one: libc::c_int = 1;
        check(libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_FREEBIND,
            (&one as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ))?;
        let sa = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr { s_addr: u32::from(addr).to_be() },
            sin_zero: [0; 8],
        };
        check(libc::bind(
            socket.as_raw_fd(),
            (&sa as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ))?;
        Ok(socket)
    }
}

/// Answers on `addr`:53, ready before the hotspot (and its address) comes up
pub fn spawn(addr: Ipv4Addr) {
    tokio::spawn(async move {
        loop {
            let socket = match bind_early(addr, 53).and_then(tokio::net::UdpSocket::from_std) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("captive DNS on {addr}:53: {e}");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
            };
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, peer)) = socket.recv_from(&mut buf).await else { break };
                if let Some(reply) = answer(&buf[..n], addr) {
                    let _ = socket.send_to(&reply, peer).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query for `name` of type `qtype`, as a resolver sends it (with an EDNS record)
    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 1];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&[0, 1]);
        q.extend_from_slice(&[0, 0, 41, 0x10, 0, 0, 0, 0, 0, 0, 0]); // OPT
        q
    }

    const BOX: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);

    #[test]
    fn every_name_is_the_box() {
        let q = query("connectivitycheck.gstatic.com", 1);
        let r = answer(&q, BOX).unwrap();
        assert_eq!(&r[..2], &[0x12, 0x34], "same id");
        assert_eq!(&r[2..4], &[0x85, 0x00], "response, authoritative, recursion desired, no error");
        assert_eq!(&r[4..12], &[0, 1, 0, 1, 0, 0, 0, 0]);
        let question_end = 12 + "connectivitycheck.gstatic.com".len() + 2 + 4;
        assert_eq!(&r[12..question_end], &q[12..question_end]);
        assert_eq!(&r[question_end..], &[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 10, 0, 4, 192, 168, 4, 1]);
    }

    #[test]
    fn other_types_get_no_answers() {
        let r = answer(&query("captive.apple.com", 28), BOX).unwrap(); // AAAA
        assert_eq!(&r[2..12], &[0x85, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r.len(), 12 + "captive.apple.com".len() + 2 + 4);
    }

    #[tokio::test]
    async fn answers_on_an_address_before_it_exists() {
        // 192.0.2.1 (documentation range) isn't on this machine; binding still works
        assert!(bind_early(Ipv4Addr::new(192, 0, 2, 1), 0).is_ok());
        let socket = tokio::net::UdpSocket::from_std(bind_early(Ipv4Addr::LOCALHOST, 0).unwrap()).unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let (n, peer) = socket.recv_from(&mut buf).await.unwrap();
            socket.send_to(&answer(&buf[..n], BOX).unwrap(), peer).await.unwrap();
        });
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(&query("example.com", 1), addr).await.unwrap();
        let mut buf = [0u8; 512];
        let n = client.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[n - 4..n], &[192, 168, 4, 1]);
    }

    #[test]
    fn ignores_junk_and_responses() {
        assert!(answer(&[0; 5], BOX).is_none());
        let mut response = query("x.y", 1);
        response[2] |= 0x80;
        assert!(answer(&response, BOX).is_none());
        let truncated = &query("example.com", 1)[..20];
        assert!(answer(truncated, BOX).is_none());
        let mut two_questions = query("x.y", 1);
        two_questions[5] = 2;
        assert_eq!(answer(&two_questions, BOX).unwrap()[3] & 0xf, 4, "not implemented");
    }
}
