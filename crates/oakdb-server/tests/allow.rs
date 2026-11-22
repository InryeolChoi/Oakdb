//! The allowlist of client addresses (design 0020, section 8-1): how its
//! entries are written, which addresses they let in, and where the server
//! listens because of them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use oakdb_server::allow::{Allowlist, Ipv4Net};

fn net(s: &str) -> Ipv4Net {
    s.parse().unwrap()
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn an_address_alone_is_that_one_address() {
    let n = net("10.0.0.5");
    assert!(n.contains("10.0.0.5".parse().unwrap()));
    assert!(!n.contains("10.0.0.6".parse().unwrap()));
    assert_eq!(n, net("10.0.0.5/32"));
}

#[test]
fn a_cidr_block_holds_the_addresses_that_share_its_prefix() {
    let n = net("192.168.1.0/24");
    assert!(n.contains("192.168.1.0".parse().unwrap()));
    assert!(n.contains("192.168.1.255".parse().unwrap()));
    assert!(!n.contains("192.168.0.255".parse().unwrap()));
    assert!(!n.contains("192.168.2.0".parse().unwrap()));

    let n = net("10.0.0.0/8");
    assert!(n.contains("10.255.255.255".parse().unwrap()));
    assert!(!n.contains("11.0.0.0".parse().unwrap()));
}

#[test]
fn prefix_zero_holds_every_address() {
    let n = net("0.0.0.0/0");
    assert!(n.contains(Ipv4Addr::new(0, 0, 0, 0)));
    assert!(n.contains(Ipv4Addr::new(255, 255, 255, 255)));
}

#[test]
fn entries_that_are_not_an_ipv4_block_are_refused() {
    for bad in [
        "",
        "10.0.0",
        "10.0.0.256",
        "10.0.0.0/33",
        "10.0.0.0/",
        "10.0.0.0/x",
        "host.example",
    ] {
        assert!(bad.parse::<Ipv4Net>().is_err(), "{bad:?} parsed");
    }
}

#[test]
fn a_block_with_host_bits_set_is_refused() {
    let err = "10.0.0.5/24".parse::<Ipv4Net>().unwrap_err();
    assert!(err.contains("10.0.0.0/24"), "{err}");
}

#[test]
fn ipv6_entries_are_refused_as_not_supported_yet() {
    for v6 in ["::1", "fe80::/10", "2001:db8::1"] {
        let err = v6.parse::<Ipv4Net>().unwrap_err();
        assert!(err.contains("IPv6"), "{v6}: {err}");
    }
}

#[test]
fn a_file_lists_one_entry_per_line_with_comments_and_blank_lines() {
    let text = "# app servers\n10.0.0.5\n\n  192.168.1.0/24   # office\n\t\n";
    assert_eq!(
        Allowlist::parse_file(text).unwrap(),
        vec![net("10.0.0.5"), net("192.168.1.0/24")]
    );
}

#[test]
fn a_bad_line_in_a_file_is_named_by_its_number() {
    let err = Allowlist::parse_file("10.0.0.5\n# ok\n10.0.0.300\n").unwrap_err();
    assert_eq!(err.line, 3);
    assert_eq!(err.text, "10.0.0.300");
}

#[test]
fn with_no_list_only_loopback_clients_are_let_in() {
    let list = Allowlist::loopback_only();
    assert!(list.allows(ip("127.0.0.1")));
    assert!(list.allows(ip("127.4.5.6")));
    assert!(!list.allows(ip("10.0.0.5")));
    assert!(!list.allows(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    assert_eq!(list.bind_ip(), Ipv4Addr::LOCALHOST);
}

#[test]
fn a_list_lets_in_its_addresses_and_loopback_and_nothing_else() {
    let list = Allowlist::new(vec![net("10.0.0.0/24")]);
    assert!(list.allows(ip("10.0.0.77")));
    assert!(list.allows(ip("127.0.0.1")));
    assert!(!list.allows(ip("10.0.1.1")));
    assert!(!list.allows(ip("192.168.1.1")));
}

#[test]
fn an_ipv4_address_written_as_ipv6_is_matched_as_ipv4() {
    let list = Allowlist::new(vec![net("10.0.0.0/24")]);
    let mapped = IpAddr::V6(Ipv4Addr::new(10, 0, 0, 9).to_ipv6_mapped());
    assert!(list.allows(mapped));
    let mapped_loopback = IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped());
    assert!(Allowlist::loopback_only().allows(mapped_loopback));
}

#[test]
fn the_server_listens_wider_only_when_the_list_names_an_outside_address() {
    assert_eq!(
        Allowlist::new(vec![net("127.0.0.0/8")]).bind_ip(),
        Ipv4Addr::LOCALHOST
    );
    assert_eq!(
        Allowlist::new(vec![net("10.0.0.5")]).bind_ip(),
        Ipv4Addr::UNSPECIFIED
    );
    assert_eq!(
        Allowlist::new(vec![net("127.0.0.0/8"), net("10.0.0.5")]).bind_ip(),
        Ipv4Addr::UNSPECIFIED
    );
}

#[test]
fn a_block_that_reaches_past_loopback_counts_as_outside() {
    // 0.0.0.0/0 and 96.0.0.0/3 hold loopback, but much more too.
    assert_eq!(
        Allowlist::new(vec![net("96.0.0.0/3")]).bind_ip(),
        Ipv4Addr::UNSPECIFIED
    );
    let everyone = Allowlist::new(vec![net("0.0.0.0/0")]);
    assert!(everyone.allows_everyone());
    assert!(!Allowlist::new(vec![net("10.0.0.0/8")]).allows_everyone());
}

#[test]
fn a_strict_list_does_not_add_loopback() {
    let list = Allowlist::strict(vec![net("127.0.0.2")]);
    assert!(!list.allows(ip("127.0.0.1")));
    assert!(list.allows(ip("127.0.0.2")));
    assert_eq!(list.bind_ip(), Ipv4Addr::LOCALHOST);
}
