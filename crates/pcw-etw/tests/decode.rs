use pcw_etw::decode::{
    guess_direction, kernel_network_kind, parse_dns_results, port_from_network_order,
    snapshot_direction,
};
use pcw_types::{Direction, EventKind, Protocol};
use std::collections::HashSet;

#[test]
fn ports_are_converted_from_network_order() {
    assert_eq!(port_from_network_order(0xBB01), 443);
}

#[test]
fn kernel_network_ids_map_to_kinds() {
    assert_eq!(
        kernel_network_kind(12),
        Some((Protocol::Tcp, Some(Direction::Out), EventKind::Open))
    );
    assert_eq!(
        kernel_network_kind(28),
        Some((Protocol::Tcp, Some(Direction::Out), EventKind::Open))
    );
    assert_eq!(
        kernel_network_kind(15),
        Some((Protocol::Tcp, Some(Direction::In), EventKind::Open))
    );
    assert_eq!(
        kernel_network_kind(31),
        Some((Protocol::Tcp, Some(Direction::In), EventKind::Open))
    );
    assert_eq!(
        kernel_network_kind(13),
        Some((Protocol::Tcp, None, EventKind::Close))
    );
    assert_eq!(
        kernel_network_kind(29),
        Some((Protocol::Tcp, None, EventKind::Close))
    );
    assert_eq!(
        kernel_network_kind(42),
        Some((Protocol::Udp, Some(Direction::Out), EventKind::Packet))
    );
    assert_eq!(
        kernel_network_kind(58),
        Some((Protocol::Udp, Some(Direction::Out), EventKind::Packet))
    );
    assert_eq!(
        kernel_network_kind(43),
        Some((Protocol::Udp, Some(Direction::In), EventKind::Packet))
    );
    assert_eq!(
        kernel_network_kind(59),
        Some((Protocol::Udp, Some(Direction::In), EventKind::Packet))
    );
    assert_eq!(kernel_network_kind(10), None); // data events never used
    assert_eq!(kernel_network_kind(11), None);
    assert_eq!(kernel_network_kind(3008), None);
}

#[test]
fn dns_results_parse_ips_and_skip_cnames() {
    assert_eq!(
        parse_dns_results("type:  5 github.com;::ffff:140.82.112.3;"),
        vec!["140.82.112.3"]
    );
    assert_eq!(
        parse_dns_results("2606:4700::6810:84e5;104.16.132.229;"),
        vec!["2606:4700::6810:84e5", "104.16.132.229"]
    );
    assert!(parse_dns_results("").is_empty());
}

#[test]
fn snapshot_and_guessed_directions() {
    let mut listening = HashSet::new();
    listening.insert(443u16);
    assert_eq!(snapshot_direction(443, &listening), Direction::In);
    assert_eq!(snapshot_direction(50000, &listening), Direction::Out);
    assert_eq!(guess_direction(51515), Direction::Out);
    assert_eq!(guess_direction(443), Direction::In);
}
