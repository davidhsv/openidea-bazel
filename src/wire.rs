//! Binary protobuf files the aspect points at: jdeps (`blaze_deps.Dependencies`) and package manifests.

use crate::types::ArtifactLocation;

/// The exec-root-relative jar paths a jdeps file lists as EXPLICIT or IMPLICIT, in file order.
pub fn parse_jdeps(bytes: &[u8]) -> Result<Vec<String>, String> {
    let _ = bytes;
    unimplemented!("agent B")
}

/// (package, source file) pairs of a `PackageManifest`.
pub fn parse_package_manifest(bytes: &[u8]) -> Result<Vec<(String, ArtifactLocation)>, String> {
    let _ = bytes;
    unimplemented!("agent B")
}
