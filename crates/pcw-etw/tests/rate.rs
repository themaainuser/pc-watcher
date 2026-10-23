use pcw_etw::rate::RateLimiter;
use std::net::{IpAddr, Ipv4Addr};

#[test]
fn cap_zero_allows_everything() {
    let mut r = RateLimiter::new(0);
    let ip: IpAddr = Ipv4Addr::new(1, 2, 3, 4).into();
    for _ in 0..1000 {
        assert!(r.allow(1, ip, 53, 1000));
    }
    assert_eq!(r.capped, 0);
}

#[test]
fn cap_five_per_second() {
    let mut r = RateLimiter::new(5);
    let ip: IpAddr = Ipv4Addr::new(8, 8, 8, 8).into();
    for _ in 0..5 {
        assert!(r.allow(1, ip, 53, 0));
    }
    assert!(!r.allow(1, ip, 53, 0));
    assert_eq!(r.capped, 1);
    // A different key is a separate bucket.
    assert!(r.allow(2, ip, 53, 0));
    // Next window: allowed again.
    assert!(r.allow(1, ip, 53, 1000));
    assert_eq!(r.capped, 1);
}
