use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{SystemTime, UNIX_EPOCH};

use aster_ipc::launch::{issue_launch_token, LaunchTokenClaims, LaunchTokenKey};

const MAX_TTL_SECONDS: u64 = 300;

fn main() {
    if let Err(error) = run() {
        eprintln!("aster_launch_token: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let cell_id = required_arg(&mut args, "cell-id")?;
    let tenant = required_arg(&mut args, "tenant")?;
    let deployment = required_arg(&mut args, "deployment")?;
    let lease_epoch = resolve_epoch(&required_arg(&mut args, "lease-epoch")?)?;
    let ttl_seconds: u64 = required_arg(&mut args, "ttl-seconds")?.parse()?;
    if args.next().is_some() {
        return Err(usage("too many arguments").into());
    }
    if ttl_seconds == 0 || ttl_seconds > MAX_TTL_SECONDS {
        return Err(
            format!("ttl-seconds must be in 1..={MAX_TTL_SECONDS}, got {ttl_seconds}").into(),
        );
    }

    let key_path = std::env::var("ASTER_LAUNCH_KEY_FILE")
        .map_err(|_| "missing required env ASTER_LAUNCH_KEY_FILE")?;
    let metadata = fs::metadata(&key_path)?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "ASTER_LAUNCH_KEY_FILE={key_path} has mode {mode:04o}; use 0400 or 0600"
        )
        .into());
    }
    let bytes = fs::read(&key_path)?;
    let key: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("launch key is {} bytes, expected 32 raw bytes", bytes.len()))?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let expires_at_unix_s = now
        .checked_add(ttl_seconds)
        .ok_or("launch token expiry overflow")?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)?;
    let token = issue_launch_token(
        &LaunchTokenKey::from_bytes(key),
        &LaunchTokenClaims {
            cell_id,
            tenant,
            deployment,
            lease_epoch,
            expires_at_unix_s,
            nonce,
        },
    )?;
    println!("{token}");
    Ok(())
}

fn resolve_epoch(value: &str) -> Result<u64, Box<dyn std::error::Error>> {
    if value != "current" {
        return Ok(value.parse()?);
    }
    let path = std::env::var("ASTER_AUTHORITY_EPOCH_FILE")
        .map_err(|_| "lease-epoch=current requires ASTER_AUTHORITY_EPOCH_FILE")?;
    let epoch = fs::read_to_string(&path)
        .map_err(|error| format!("read ASTER_AUTHORITY_EPOCH_FILE={path}: {error}"))?;
    Ok(epoch.trim().parse()?)
}

fn required_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next().ok_or_else(|| usage(&format!("missing {name}")))
}

fn usage(error: &str) -> String {
    format!(
        "{error}\nusage: aster_launch_token <cell-id> <tenant> <deployment> \
         <lease-epoch|current> <ttl-seconds>"
    )
}
