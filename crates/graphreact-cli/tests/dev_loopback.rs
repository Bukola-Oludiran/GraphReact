use graphreact_cli::commands::dev_loopback_cluster_material;

#[test]
fn generated_material_is_ephemeral_loopback_and_mtls() {
    let (node, listen, _cookie, tls) = dev_loopback_cluster_material().expect("material");
    assert_eq!(node.to_string(), "greenhouse@127.0.0.1");
    assert!(listen.ip().is_loopback());
    assert_eq!(listen.port(), 0);
    assert!(tls.require_mtls);
}
