use alloy::primitives::address;
use nft_mint_bot::setup::{bind_manual_control, cleanup_manual_control, send_manual_trigger};
use nft_mint_bot::{abi::encode_mint, config::MintCallConfig};

#[test]
fn dynamic_calldata_supports_legitimate_merkle_proofs() {
    let call = MintCallConfig {
        function: "mint(uint256,bytes32[])".to_string(),
        arguments: vec!["$quantity".to_string(), "$proof".to_string()],
        proof: Some(vec![
            "0x0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        ]),
        price_per_nft: "0".to_string(),
    };
    let calldata = encode_mint(
        &call,
        2,
        address!("0x0000000000000000000000000000000000000001"),
        call.proof.as_deref(),
    )
    .expect("proof calldata should encode");
    assert_eq!(&calldata.bytes[..4], &calldata.function.selector().0);
    assert!(calldata.bytes.len() > 4 + 64);
}

#[tokio::test]
async fn authenticated_manual_control_delivers_exactly_one_trigger() {
    // Some CI/sandbox profiles prohibit even loopback listeners. Probe that
    // capability separately so such an environment does not disguise a
    // control-file or authentication regression as a product failure.
    match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(probe) => drop(probe),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping loopback test because this sandbox forbids local listeners");
            return;
        }
        Err(err) => panic!("loopback listener probe failed: {err}"),
    }
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("manual.json");
    std::fs::write(&config_path, "{}").unwrap();
    let (mut receiver, control_path) = bind_manual_control(&config_path).await.unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&control_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    send_manual_trigger(&config_path).await.unwrap();
    let trigger = tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .expect("manual trigger should arrive");
    assert_eq!(trigger, Some(()));
    cleanup_manual_control(&control_path);
}
