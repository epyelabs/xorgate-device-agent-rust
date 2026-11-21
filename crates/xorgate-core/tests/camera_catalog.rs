//! The vendored sensor catalog (`crates/xorgate-core/data/camera-catalog.json`)
//! is a GENERATED file: the backend's `npm run contract:generate` writes it
//! (plan `support-other-camera-models`, PROGRESS.md "Implementation log").
//! This test pins its bytes, so a hand edit, a half-applied regenerate or a
//! catalog change that nobody looked at fails here.
//!
//! When the catalog changes on purpose: regenerate, then move [`SHA256`] and
//! `catalogVersion`, regenerate `system/xorgate-camcfg.overlays` from the
//! text this test prints, update `system/SHA256SUMS`, and read the diff of
//! the catalog tests in `src/camera_catalog.rs` and `src/boot_config.rs`
//! (default rotations, `canFlip`, the 98-character line limit).

use sha2::{Digest, Sha256};
use xorgate_core::camera_catalog::{catalog, loaded, parse_catalog, CATALOG_JSON, SCHEMA_VERSION};

/// sha256 of the vendored file (backend `camera/phase1-catalog` @ `b59d7d4`).
const SHA256: &str = "812ce3d6c2c4d1de6209c0ec118a9ecbac998b5cf23e5ccf27bd4fbb8c4aafb3";

fn repo_file(rel: &str) -> String {
    let path = format!("{}/../../{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn the_vendored_catalog_is_the_generated_file() {
    assert_eq!(hex::encode(Sha256::digest(CATALOG_JSON.as_bytes())), SHA256);
    // What is compiled in is the file in the tree.
    assert_eq!(
        CATALOG_JSON,
        repo_file("crates/xorgate-core/data/camera-catalog.json")
    );
    let doc: serde_json::Value = serde_json::from_str(CATALOG_JSON).unwrap();
    assert_eq!(doc["schemaVersion"], SCHEMA_VERSION);
    assert_eq!(doc["catalogVersion"], 3);
    assert_eq!(
        doc["generatedBy"],
        "backend/xorgate-core-service: npm run contract:generate"
    );
}

#[test]
fn every_entry_of_the_vendored_catalog_is_usable_by_this_build() {
    let (c, err) = loaded();
    assert_eq!(*err, None);
    assert_eq!(c.skipped, vec![], "an entry this build cannot read");
    let doc: serde_json::Value = serde_json::from_str(CATALOG_JSON).unwrap();
    assert_eq!(c.sensors.len(), doc["sensors"].as_array().unwrap().len());
    assert_eq!(
        c.package_sets.len(),
        doc["packageSets"].as_array().unwrap().len()
    );
    assert_eq!(parse_catalog(CATALOG_JSON).as_ref(), Ok(c));
    // No two sensors share a libcamera name or a device-tree node name: the
    // lookups would otherwise depend on the file order.
    for (i, a) in c.sensors.iter().enumerate() {
        for b in &c.sensors[i + 1..] {
            for n in &a.libcamera_names {
                assert!(!b.libcamera_names.contains(n), "{n}: {} and {}", a.id, b.id);
            }
            for n in &a.dt_node_names {
                assert!(!b.dt_node_names.contains(n), "{n}: {} and {}", a.id, b.id);
            }
        }
    }
}

/// The helper's allow-list is generated from the catalog: the committed
/// file is the text `Catalog::helper_allow_list` renders, byte for byte.
#[test]
fn the_helpers_allow_list_is_generated_from_the_catalog() {
    let want = catalog().helper_allow_list();
    let have = repo_file("system/xorgate-camcfg.overlays");
    assert_eq!(
        have, want,
        "system/xorgate-camcfg.overlays is stale; it must be:\n{want}"
    );
    let names: Vec<&str> = have.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(names, ["imx290", "imx219", "imx708", "imx335"]);
    assert!(have.contains("# catalogVersion 3\n"));
}
