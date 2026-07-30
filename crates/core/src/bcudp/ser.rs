use super::{crc::calc_crc, model::*, xml_crypto::encrypt};
use crate::Error;
use cookie_factory::bytes::*;
use cookie_factory::sequence::tuple;
use cookie_factory::SerializeFn;
use cookie_factory::{combinator::*, gen};
use std::io::Write;

impl BcUdp {
    pub(crate) fn serialize<W: Write>(&self, buf: W) -> Result<W, Error> {
        let (buf, _) = match &self {
            BcUdp::Discovery(payload) => {
                let xml_payload = encrypt(payload.tid, &payload.payload.serialize(vec![]).unwrap());
                gen(bcudp_disc(payload, &xml_payload), buf)?
            }
            BcUdp::Ack(payload) => {
                let binary_payload = &payload.payload;
                gen(bcudp_ack(payload, binary_payload), buf)?
            }
            BcUdp::Data(payload) => {
                let binary_payload = &payload.payload;
                gen(bcudp_data(payload, binary_payload), buf)?
            }
        };

        Ok(buf)
    }
}

fn bcudp_disc<'a, W: 'a + Write>(
    payload: &'a UdpDiscovery,
    xml_payload: &'a [u8],
) -> impl SerializeFn<W> + 'a {
    let checksum = calc_crc(xml_payload);
    tuple((
        le_u32(MAGIC_HEADER_UDP_NEGO),
        le_u32(xml_payload.len() as u32),
        le_u32(1),
        le_u32(payload.tid),
        le_u32(checksum),
        slice(xml_payload),
    ))
}

fn bcudp_ack<'a, W: 'a + Write>(
    payload: &'a UdpAck,
    binary_payload: &'a [u8],
) -> impl SerializeFn<W> + 'a {
    tuple((
        le_u32(MAGIC_HEADER_UDP_ACK),
        le_i32(payload.connection_id),
        le_u32(0),
        le_u32(payload.group_id),
        le_u32(payload.packet_id),
        le_u32(payload.maybe_latency),
        le_u32(binary_payload.len() as u32),
        slice(binary_payload),
    ))
}

fn bcudp_data<'a, W: 'a + Write>(
    payload: &'a UdpData,
    binary_payload: &'a [u8],
) -> impl SerializeFn<W> + 'a {
    tuple((
        le_u32(MAGIC_HEADER_UDP_DATA),
        le_i32(payload.connection_id),
        le_u32(0),
        le_u32(payload.packet_id),
        le_u32(binary_payload.len() as u32),
        slice(binary_payload),
    ))
}

#[cfg(test)]
mod tests {
    use crate::bcudp::model::*;
    use bytes::BytesMut;
    use env_logger::Env;

    fn init() {
        let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info"))
            .is_test(true)
            .try_init();
    }

    #[test]
    // Tests the decoding of a UdpDiscovery with a discovery xml
    fn test_nego_disconnect() {
        init();

        let sample = include_bytes!("samples/udp_negotiate_disc.bin");

        let msg = BcUdp::deserialize(&mut BytesMut::from(&sample[..])).unwrap();
        let ser_buf: Vec<u8> = msg.serialize(vec![]).unwrap();
        let msg2 = BcUdp::deserialize(&mut BytesMut::from(ser_buf.as_slice())).unwrap();
        assert_eq!(msg, msg2);
        // Raw samples don't quite match exactly
        // because the serde for xml puts spaces and new lines in different places
        // then the raw data from the camera so we skip this last assert
        //assert_eq!(&sample[..], ser_buf.as_slice());
    }

    #[test]
    // Tests the decoding of a UdpDiscovery with a Camera Transmission xml
    fn test_nego_cam_transmission() {
        init();

        let sample = include_bytes!("samples/udp_negotiate_camt.bin");

        let msg = BcUdp::deserialize(&mut BytesMut::from(&sample[..])).unwrap();
        let ser_buf = msg.serialize(vec![]).unwrap();
        let msg2 = BcUdp::deserialize(&mut BytesMut::from(ser_buf.as_slice())).unwrap();
        assert_eq!(msg, msg2);
        // Raw samples don't quite match exactly
        // because the serde for xml puts spaces and new lines in different places
        // then the raw data from the camera so we skip this last assert
        //assert_eq!(&sample[..], ser_buf.as_slice());
    }

    #[test]
    // Tests the decoding of a UdpDiscovery with a Client Transmission xml
    fn test_nego_client_transmission() {
        init();

        let sample = include_bytes!("samples/udp_negotiate_clientt.bin");

        let msg = BcUdp::deserialize(&mut BytesMut::from(&sample[..])).unwrap();
        let ser_buf = msg.serialize(vec![]).unwrap();
        let msg2 = BcUdp::deserialize(&mut BytesMut::from(ser_buf.as_slice())).unwrap();
        assert_eq!(msg, msg2);
        // Raw samples don't quite match exactly
        // because the serde for xml puts spaces and new lines in different places
        // then the raw data from the camera so we skip this last assert
        //assert_eq!(&sample[..], ser_buf.as_slice());
    }

    #[test]
    // Tests the decoding of a UdpDiscovery with a Camera CFM xml
    fn test_nego_cfm() {
        init();

        let sample = include_bytes!("samples/udp_negotiate_camcfm.bin");

        let msg = BcUdp::deserialize(&mut BytesMut::from(&sample[..])).unwrap();
        let ser_buf = msg.serialize(vec![]).unwrap();
        let msg2 = BcUdp::deserialize(&mut BytesMut::from(ser_buf.as_slice())).unwrap();
        assert_eq!(msg, msg2);
        // Raw samples don't quite match exactly
        // because the serde for xml puts spaces and new lines in different places
        // then the raw data from the camera so we skip this last assert
        //assert_eq!(&sample[..], ser_buf.as_slice());
    }

    #[test]
    // Tests the decoding of an acknoledge packet
    fn test_ack() {
        init();

        let sample = include_bytes!("samples/udp_ack.bin");

        let msg = BcUdp::deserialize(&mut BytesMut::from(&sample[..])).unwrap();
        let ser_buf = msg.serialize(vec![]).unwrap();
        let msg2 = BcUdp::deserialize(&mut BytesMut::from(ser_buf.as_slice())).unwrap();
        assert_eq!(msg, msg2);
        assert_eq!(&sample[..], ser_buf.as_slice());
    }

    #[test]
    // Tests the decoding of an data packet
    fn test_data() {
        init();

        let sample = include_bytes!("samples/udp_data.bin");

        let msg = BcUdp::deserialize(&mut BytesMut::from(&sample[..])).unwrap();
        let ser_buf = msg.serialize(vec![]).unwrap();
        let msg2 = BcUdp::deserialize(&mut BytesMut::from(ser_buf.as_slice())).unwrap();
        assert_eq!(msg, msg2);
        assert_eq!(&sample[..], ser_buf.as_slice());
    }
}

/// Golden-byte snapshots of the XML serializer.
///
/// The four discovery tests above are round-trip only: they prove
/// `deserialize(serialize(x)) == x`, which stays true even if the emitted bytes
/// change completely. They cannot be tightened into byte comparisons against the
/// captured samples, for the reason their own comments give -- quick-xml puts
/// whitespace in different places than the camera does, so
/// `assert_eq!(&sample[..], ser_buf.as_slice())` has been commented out in all
/// four since they were written.
///
/// That leaves the emitted bytes untested, and they matter: the payload is fed
/// through `calc_crc` (`bcudp/ser.rs:34`) into a checksum the camera validates.
/// A serializer that starts emitting a newline or an XML declaration would keep
/// every round-trip test green while every camera rejected our packets.
///
/// So instead of comparing against the camera's bytes, compare against *our own*
/// current bytes. These snapshots were captured from the implementation as it
/// stands on quick-xml 0.36.2. If a quick-xml upgrade changes them, that is not
/// necessarily a bug -- but it must be a decision, taken with the CRC in mind,
/// rather than something that slips through unnoticed.
#[cfg(test)]
mod golden {
    use crate::bcudp::model::*;
    use bytes::BytesMut;
    use std::convert::TryInto;

    /// `le_u32` fields: magic, payload length, `01000000`, tid, then checksum.
    const CRC_OFFSET: usize = 16;

    fn check(sample: &[u8], expected_xml: &str, expected_crc: u32) {
        let msg = BcUdp::deserialize(&mut BytesMut::from(sample)).unwrap();
        let BcUdp::Discovery(disc) = &msg else {
            panic!("sample is not a UdpDiscovery");
        };

        let xml = disc.payload.serialize(vec![]).unwrap();
        assert_eq!(
            String::from_utf8(xml).unwrap(),
            expected_xml,
            "serialized XML changed"
        );

        let packet: Vec<u8> = msg.serialize(vec![]).unwrap();
        let crc = u32::from_le_bytes(packet[CRC_OFFSET..CRC_OFFSET + 4].try_into().unwrap());
        assert_eq!(
            crc, expected_crc,
            "payload checksum changed -- the camera validates this"
        );
    }

    #[test]
    fn golden_nego_disconnect() {
        check(
            &include_bytes!("samples/udp_negotiate_disc.bin")[..],
            "<P2P><C2D_DISC><cid>82000</cid><did>80</did></C2D_DISC></P2P>",
            0x7400_3833,
        );
    }

    #[test]
    fn golden_nego_cam_transmission() {
        check(
            &include_bytes!("samples/udp_negotiate_camt.bin")[..],
            "<P2P><D2C_T><sid>62098713</sid><conn>local</conn><cid>82001</cid>\
             <did>96</did></D2C_T></P2P>",
            0xD47B_885C,
        );
    }

    #[test]
    fn golden_nego_client_transmission() {
        check(
            &include_bytes!("samples/udp_negotiate_clientt.bin")[..],
            "<P2P><C2D_T><sid>62098713</sid><conn>local</conn><cid>82001</cid>\
             <mtu>1350</mtu></C2D_T></P2P>",
            0x14CA_129F,
        );
    }

    #[test]
    fn golden_nego_cfm() {
        check(
            &include_bytes!("samples/udp_negotiate_camcfm.bin")[..],
            "<P2P><D2C_CFM><sid>62098713</sid><conn>local</conn><rsp>0</rsp>\
             <cid>82001</cid><did>96</did><time_r>0</time_r></D2C_CFM></P2P>",
            0x2515_E098,
        );
    }

    /// The serializer must emit no XML declaration and no trailing newline.
    /// `UdpXml::serialize` explicitly comments out the `BytesDecl` write; this
    /// pins that decision, since re-enabling it would shift every checksum.
    #[test]
    fn golden_no_xml_declaration_or_trailing_whitespace() {
        let sample = &include_bytes!("samples/udp_negotiate_disc.bin")[..];
        let msg = BcUdp::deserialize(&mut BytesMut::from(sample)).unwrap();
        let BcUdp::Discovery(disc) = &msg else {
            panic!("sample is not a UdpDiscovery");
        };
        let xml = String::from_utf8(disc.payload.serialize(vec![]).unwrap()).unwrap();
        assert!(
            !xml.contains("<?xml"),
            "unexpected XML declaration: {}",
            xml
        );
        assert_eq!(xml.trim_end(), xml, "unexpected trailing whitespace");
        assert!(!xml.contains('\n'), "unexpected newline: {}", xml);
    }
}
