//! USB/IP 1.1.1 handshake validation and a read-only configuration probe.
//! Physical device traffic remains reliable and is never replayed after detach.
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const MAGIC: &[u8; 8] = b"FUSB0001";
pub const IMPORT_REQUEST_BYTES: usize = 40;
pub const IMPORT_REPLY_BYTES: usize = 320;

pub fn import_request(bus: &str) -> anyhow::Result<[u8; IMPORT_REQUEST_BYTES]> {
    anyhow::ensure!(crate::protocol::valid_bus_id(bus), "Invalid USB bus ID");
    let mut request = [0; IMPORT_REQUEST_BYTES];
    request[..4].copy_from_slice(&[1, 0x11, 0x80, 3]);
    request[8..8 + bus.len()].copy_from_slice(bus.as_bytes());
    Ok(request)
}
pub fn validate_import(request: &[u8; IMPORT_REQUEST_BYTES], bus: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        request == &import_request(bus)?,
        "USB proxy accepts only the selected device import"
    );
    Ok(())
}
pub fn validate_configuration(bytes: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        bytes.len() >= 9 && bytes[0] == 9 && bytes[1] == 2,
        "Invalid USB configuration descriptor"
    );
    let length = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
    anyhow::ensure!(
        length == bytes.len(),
        "Truncated USB configuration descriptor"
    );
    let mut offset = 0;
    while offset < length {
        anyhow::ensure!(offset + 2 <= length, "Invalid USB descriptor length");
        let size = bytes[offset] as usize;
        anyhow::ensure!(
            size >= 2 && offset + size <= length,
            "Invalid USB descriptor length"
        );
        if bytes[offset + 1] == 5 {
            anyhow::ensure!(size >= 7, "Invalid USB endpoint descriptor");
            anyhow::ensure!(
                bytes[offset + 3] & 3 != 1,
                "USB audio/video and other isochronous devices are not supported in this release"
            );
        }
        offset += size;
    }
    Ok(())
}
pub async fn import_and_probe(
    socket: &mut tokio::net::TcpStream,
    bus: &str,
) -> anyhow::Result<[u8; IMPORT_REPLY_BYTES]> {
    socket.write_all(&import_request(bus)?).await?;
    let mut response = [0; IMPORT_REPLY_BYTES];
    // Read status before the device body: error replies contain only eight bytes.
    socket.read_exact(&mut response[..8]).await?;
    anyhow::ensure!(
        response[..8] == [1, 0x11, 0, 3, 0, 0, 0, 0],
        "USB/IP exporter refused the selected device"
    );
    socket.read_exact(&mut response[8..]).await?;
    let bus_number = u32::from_be_bytes(response[296..300].try_into().unwrap());
    let device_number = u32::from_be_bytes(response[300..304].try_into().unwrap());
    anyhow::ensure!(
        bus_number <= 65535 && device_number <= 65535,
        "Invalid USB device address"
    );
    let device_id = (bus_number << 16) | device_number;
    let configurations = response[318];
    anyhow::ensure!(
        (1..=8).contains(&configurations),
        "Unsupported USB configuration count"
    );
    for index in 0..configurations {
        let header = descriptor(socket, device_id, index, 9).await?;
        anyhow::ensure!(header.len() == 9, "Invalid USB configuration header");
        let length = u16::from_le_bytes([header[2], header[3]]);
        anyhow::ensure!(length >= 9, "Invalid USB configuration length");
        let bytes = descriptor(socket, device_id, index, length).await?;
        validate_configuration(&bytes)?;
    }
    Ok(response)
}
async fn descriptor(
    socket: &mut tokio::net::TcpStream,
    device: u32,
    index: u8,
    size: u16,
) -> anyhow::Result<Vec<u8>> {
    let sequence = 0xf0000000 | (index as u32) << 16 | size as u32;
    let mut request = [0u8; 48];
    for (offset, value) in [
        (0, 1u32),
        (4, sequence),
        (8, device),
        (12, 1),
        (24, size as u32),
    ] {
        request[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    request[40..].copy_from_slice(&[0x80, 6, index, 2, 0, 0, size as u8, (size >> 8) as u8]);
    socket.write_all(&request).await?;
    let mut reply = [0u8; 48];
    socket.read_exact(&mut reply).await?;
    let word = |offset| u32::from_be_bytes(reply[offset..offset + 4].try_into().unwrap());
    anyhow::ensure!(
        word(0) == 3
            && word(4) == sequence
            && word(20) == 0
            && word(24) <= size as u32
            && word(32) == 0,
        "USB configuration probe failed"
    );
    let mut bytes = vec![0; word(24) as usize];
    socket.read_exact(&mut bytes).await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proxy_import_cannot_select_a_different_device() {
        assert!(validate_import(&import_request("1-2").unwrap(), "1-2").is_ok());
        assert!(validate_import(&import_request("1-3").unwrap(), "1-2").is_err());
    }
    #[test]
    fn configuration_rejects_isochronous_and_malformed_endpoints() {
        let mut descriptor = vec![9, 2, 16, 0, 1, 1, 0, 0x80, 50, 7, 5, 0x81, 2, 64, 0, 0];
        validate_configuration(&descriptor).unwrap();
        descriptor[12] = 1;
        assert!(validate_configuration(&descriptor).is_err());
        descriptor[12] = 2;
        descriptor[9] = 0;
        assert!(validate_configuration(&descriptor).is_err());
    }

    async fn probe_fixture(endpoint_type: u8) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; IMPORT_REQUEST_BYTES];
            socket.read_exact(&mut request).await.unwrap();
            validate_import(&request, "1-2").unwrap();
            let mut reply = [0; IMPORT_REPLY_BYTES];
            reply[..4].copy_from_slice(&[1, 0x11, 0, 3]);
            reply[296..300].copy_from_slice(&1u32.to_be_bytes());
            reply[300..304].copy_from_slice(&2u32.to_be_bytes());
            reply[318] = 1;
            socket.write_all(&reply).await.unwrap();
            let descriptor = [
                9,
                2,
                16,
                0,
                1,
                1,
                0,
                0x80,
                50,
                7,
                5,
                0x81,
                endpoint_type,
                64,
                0,
                0,
            ];
            for size in [9usize, 16] {
                let mut request = [0u8; 48];
                socket.read_exact(&mut request).await.unwrap();
                assert_eq!(request[40..44], [0x80, 6, 0, 2]);
                assert_eq!(
                    u32::from_be_bytes(request[24..28].try_into().unwrap()) as usize,
                    size
                );
                let mut response = [0u8; 48];
                response[..4].copy_from_slice(&3u32.to_be_bytes());
                response[4..8].copy_from_slice(&request[4..8]);
                response[24..28].copy_from_slice(&(size as u32).to_be_bytes());
                socket.write_all(&response).await.unwrap();
                socket.write_all(&descriptor[..size]).await.unwrap();
            }
        });
        let mut socket = tokio::net::TcpStream::connect(address).await?;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            import_and_probe(&mut socket, "1-2"),
        )
        .await?;
        server.await?;
        result.map(|_| ())
    }
    #[tokio::test]
    async fn real_tcp_probe_reads_bulk_descriptors_and_rejects_isochronous() {
        probe_fixture(2).await.unwrap();
        assert!(
            probe_fixture(1)
                .await
                .unwrap_err()
                .to_string()
                .contains("isochronous")
        );
    }
    #[tokio::test]
    async fn short_import_error_reply_does_not_wait_for_a_device_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; IMPORT_REQUEST_BYTES];
            socket.read_exact(&mut request).await.unwrap();
            socket
                .write_all(&[1, 0x11, 0, 3, 0, 0, 0, 1])
                .await
                .unwrap();
        });
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                import_and_probe(&mut socket, "1-2")
            )
            .await
            .unwrap()
            .is_err()
        );
        server.await.unwrap();
    }
}
