use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use aster_ipc::launch::{verify_launch_token, LaunchTokenKey};

#[test]
fn cli_issues_short_lived_token_bound_to_published_epoch() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let temp = std::env::temp_dir().join(format!("aster-launch-cli-{nonce}"));
    fs::create_dir_all(&temp).expect("create temp dir");
    let key_path = temp.join("launch.key");
    let key = [0x61_u8; 32];
    fs::write(&key_path, key).expect("write launch key");
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
        .expect("restrict key permissions");
    let epoch_path = temp.join("authority_epoch");
    fs::write(&epoch_path, "9\n").expect("write authority epoch");

    let output = Command::new(env!("CARGO_BIN_EXE_aster_launch_token"))
        .env("ASTER_LAUNCH_KEY_FILE", &key_path)
        .env("ASTER_AUTHORITY_EPOCH_FILE", &epoch_path)
        .args(["cell-cli", "tenant-cli", "dep-cli", "current", "60"])
        .output()
        .expect("run token issuer");
    assert!(
        output.status.success(),
        "issuer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let token = String::from_utf8(output.stdout)
        .expect("token utf8")
        .trim()
        .to_string();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_secs();
    let claims = verify_launch_token(&LaunchTokenKey::from_bytes(key), &token, now)
        .expect("verify issued token");
    assert_eq!(claims.cell_id, "cell-cli");
    assert_eq!(claims.tenant, "tenant-cli");
    assert_eq!(claims.deployment, "dep-cli");
    assert_eq!(claims.lease_epoch, 9);
    assert!(claims.expires_at_unix_s > now);

    fs::remove_dir_all(temp).expect("remove temp dir");
}
