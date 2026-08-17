use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::net::UdpSocket;

/// The AWS link-local time service first, because on an EC2 instance it answers
/// in about a millisecond and needs no route off the box. `time.aws.com` is the
/// fallback for everywhere else.
pub const NTP_HOSTS: [&str; 2] = ["169.254.169.123:123", "time.aws.com:123"];

/// NTP counts from 1900, Unix from 1970.
const NTP_TO_UNIX_SECONDS: u64 = 2_208_988_800;

#[derive(Debug, Error)]
pub enum NtpError {
    #[error("no time source answered")]
    NoAnswer,
    #[error("the reply was not an NTP packet")]
    Malformed,
}

/// One SNTP exchange, returning how far this machine's clock is from the
/// reference. Positive means the local clock is behind.
///
/// Uses all four timestamps rather than just the reply, so the round trip is
/// divided out instead of being charged entirely to the offset.
pub async fn sample_offset(host: &str, timeout: Duration) -> Result<i64, NtpError> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|_| NtpError::NoAnswer)?;
    socket.connect(host).await.map_err(|_| NtpError::NoAnswer)?;

    // LI = 0, VN = 3, Mode = 3 (client). Everything else zero.
    let mut request = [0u8; 48];
    request[0] = 0x1b;

    let t1 = unix_millis();
    socket
        .send(&request)
        .await
        .map_err(|_| NtpError::NoAnswer)?;

    let mut reply = [0u8; 48];
    let read = tokio::time::timeout(timeout, socket.recv(&mut reply))
        .await
        .map_err(|_| NtpError::NoAnswer)?
        .map_err(|_| NtpError::NoAnswer)?;
    let t4 = unix_millis();

    if read < 48 {
        return Err(NtpError::Malformed);
    }
    offset_from(&reply, t1, t4)
}

/// Tries each host in turn and takes the first answer.
pub async fn sample_any_offset(hosts: &[&str], timeout: Duration) -> Result<i64, NtpError> {
    for host in hosts {
        if let Ok(offset) = sample_offset(host, timeout).await {
            return Ok(offset);
        }
    }
    Err(NtpError::NoAnswer)
}

/// The offset calculation, separated from the socket so it can be tested with
/// known packets instead of a live server.
pub fn offset_from(reply: &[u8; 48], t1: i64, t4: i64) -> Result<i64, NtpError> {
    // Bytes 32..40 are receive, 40..48 are transmit. A server that answers with
    // a zero transmit timestamp is not one to set a clock by.
    let t2 = ntp_millis(reply, 32)?;
    let t3 = ntp_millis(reply, 40)?;
    if t2 == 0 || t3 == 0 {
        return Err(NtpError::Malformed);
    }
    // ((T2 - T1) + (T3 - T4)) / 2, via midpoint so the sum cannot overflow on a
    // server that answers with a wildly wrong timestamp.
    Ok(i64::midpoint(t2 - t1, t3 - t4))
}

fn ntp_millis(buf: &[u8; 48], at: usize) -> Result<i64, NtpError> {
    let seconds = u32::from_be_bytes(
        buf[at..at + 4]
            .try_into()
            .map_err(|_| NtpError::Malformed)?,
    );
    let fraction = u32::from_be_bytes(
        buf[at + 4..at + 8]
            .try_into()
            .map_err(|_| NtpError::Malformed)?,
    );
    if seconds == 0 {
        return Ok(0);
    }
    let unix = i64::from(seconds) - i64::try_from(NTP_TO_UNIX_SECONDS).unwrap_or(0);
    // The fraction is a 32 bit fixed point share of one second.
    let millis = (u64::from(fraction) * 1000) >> 32;
    Ok(unix * 1000 + i64::try_from(millis).unwrap_or(0))
}

#[must_use]
pub fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a reply as a server would, so the arithmetic is checked against
    /// known numbers rather than against whatever a live server happens to say.
    fn reply_with(receive_ms: i64, transmit_ms: i64) -> [u8; 48] {
        let mut buf = [0u8; 48];
        for (at, ms) in [(32, receive_ms), (40, transmit_ms)] {
            let secs =
                u32::try_from(ms / 1000 + i64::try_from(NTP_TO_UNIX_SECONDS).unwrap()).unwrap();
            // Parenthesised deliberately: `/` binds tighter than `<<` in Rust,
            // so `<< 32 / 1000` shifts by zero and silently drops the fraction.
            let frac =
                u32::try_from(((u64::try_from(ms % 1000).unwrap()) << 32) / 1000).unwrap_or(0);
            buf[at..at + 4].copy_from_slice(&secs.to_be_bytes());
            buf[at + 4..at + 8].copy_from_slice(&frac.to_be_bytes());
        }
        buf
    }

    #[test]
    fn a_clock_in_step_reads_as_no_offset() {
        // Sent at 1000, server saw it at 1010 and replied at 1010, back at 1020.
        // Ten milliseconds each way, so the local clock is correct.
        let reply = reply_with(1_000_010, 1_000_010);
        let offset = offset_from(&reply, 1_000_000, 1_000_020).unwrap();
        assert!(offset.abs() <= 1, "expected about zero, got {offset}");
    }

    /// The case that matters: a local clock that is behind must read positive,
    /// because the sleep has to wait longer, not shorter.
    #[test]
    fn a_clock_that_is_behind_reads_positive() {
        // The server is five seconds ahead of us.
        let reply = reply_with(1_005_010, 1_005_010);
        let offset = offset_from(&reply, 1_000_000, 1_000_020).unwrap();
        assert!(
            offset > 4_900 && offset < 5_100,
            "expected about +5000, got {offset}"
        );
    }

    #[test]
    fn a_clock_that_is_ahead_reads_negative() {
        let reply = reply_with(995_010, 995_010);
        let offset = offset_from(&reply, 1_000_000, 1_000_020).unwrap();
        assert!(
            offset < -4_900 && offset > -5_100,
            "expected about -5000, got {offset}"
        );
    }

    /// A server answering with zeros is not a server to set a clock by, and
    /// treating it as "no offset" would be the worst possible reading.
    #[test]
    fn refuses_a_reply_with_no_timestamps() {
        assert!(matches!(
            offset_from(&[0u8; 48], 0, 0),
            Err(NtpError::Malformed)
        ));
    }

    #[test]
    fn the_round_trip_is_divided_out_rather_than_charged_to_the_offset() {
        // A slow path: 200 ms each way, but the clock itself is correct.
        let reply = reply_with(1_000_200, 1_000_200);
        let offset = offset_from(&reply, 1_000_000, 1_000_400).unwrap();
        assert!(
            offset.abs() <= 2,
            "round trip leaked into the offset: {offset}"
        );
    }
}
