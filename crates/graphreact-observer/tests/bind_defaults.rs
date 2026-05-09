//! **M16-Backend P2 verification gate.** [`BindConfig::from_flags`]
//! synchronously refuses construction when remote-bind requirements
//! aren't all four met (token + cert + key + non-conflicting
//! token sources). Operator MUST see the missing flag at process
//! start, NOT during the first HTTP connect.

use std::path::PathBuf;

use graphreact_observer::{BindConfig, BindConfigError, BindFlags, DEFAULT_BIND};

#[test]
fn no_flags_yields_loopback_default() {
    let cfg = BindConfig::from_flags(BindFlags::default()).expect("default");
    assert_eq!(cfg.addr, DEFAULT_BIND);
    assert!(cfg.token.is_none());
    assert!(cfg.tls.is_none());
    assert!(cfg.is_loopback());
}

#[test]
fn remote_bind_without_token_refuses() {
    let result = BindConfig::from_flags(BindFlags {
        addr: Some("0.0.0.0:9090".parse().unwrap()),
        ..BindFlags::default()
    });
    let err = result.err().expect("expected error for remote-no-token");
    match err {
        BindConfigError::RemoteWithoutToken { addr } => {
            assert_eq!(addr.port(), 9090);
        }
        other => panic!("expected RemoteWithoutToken, got {other:?}"),
    }
}

#[test]
fn remote_bind_with_token_but_no_cert_refuses() {
    std::env::set_var("GRX_OBSERVER_TOKEN_BD_NOCERT", "secret-bytes");
    let result = BindConfig::from_flags(BindFlags {
        addr: Some("0.0.0.0:9090".parse().unwrap()),
        token_env: Some("GRX_OBSERVER_TOKEN_BD_NOCERT".into()),
        ..BindFlags::default()
    });
    std::env::remove_var("GRX_OBSERVER_TOKEN_BD_NOCERT");
    let err = result.err().expect("expected error");
    assert!(
        matches!(err, BindConfigError::RemoteWithoutCert { .. }),
        "expected RemoteWithoutCert, got {err:?}"
    );
}

#[test]
fn cert_without_key_refuses_even_on_loopback() {
    let result = BindConfig::from_flags(BindFlags {
        cert: Some(PathBuf::from("/tmp/no-such-cert.pem")),
        ..BindFlags::default()
    });
    let err = result.err().expect("expected error");
    assert!(
        matches!(err, BindConfigError::RemoteWithoutKey { .. }),
        "expected RemoteWithoutKey, got {err:?}"
    );
}

#[test]
fn token_env_and_token_file_both_set_refuses() {
    let result = BindConfig::from_flags(BindFlags {
        token_env: Some("SOME_VAR".into()),
        token_file: Some(PathBuf::from("/tmp/x")),
        ..BindFlags::default()
    });
    let err = result.err().expect("expected error");
    assert!(matches!(err, BindConfigError::TokenSourceConflict));
}

#[test]
fn explicit_loopback_with_no_auth_works() {
    let cfg = BindConfig::from_flags(BindFlags {
        addr: Some("127.0.0.1:18080".parse().unwrap()),
        ..BindFlags::default()
    })
    .expect("loopback with no auth is fine");
    assert!(cfg.is_loopback());
}

#[test]
fn ipv6_loopback_with_no_auth_works() {
    let cfg = BindConfig::from_flags(BindFlags {
        addr: Some("[::1]:18081".parse().unwrap()),
        ..BindFlags::default()
    })
    .expect("ipv6 loopback ok");
    assert!(cfg.is_loopback());
}
