//! One-off helper: encode an IDv6 string from `(table_number, hex internal_id)`.
//! Used by `docker/smoke-bundle.sh` to produce the `sourcePackageId` field
//! it writes into the `_modules` row body. Not shipped — `--example`
//! keeps it out of release builds.

use aster_convex_codec::DocumentIdV6;

fn main() {
    let mut args = std::env::args().skip(1);
    let table_number: u32 = args
        .next()
        .expect("usage: idv6_smoke_helper <table_number> <hex_internal_id_32chars>")
        .parse()
        .expect("table_number must be u32");
    let hex = args.next().expect("hex internal_id required");
    assert_eq!(hex.len(), 32, "internal_id must be 32 hex chars (16 bytes)");
    let mut internal_id = [0u8; 16];
    for (i, byte) in internal_id.iter_mut().enumerate() {
        *byte =
            u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("internal_id chars must be hex");
    }
    let id = DocumentIdV6::new(table_number, internal_id);
    println!("{}", id.encode());
}
