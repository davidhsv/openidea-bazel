//! Bazel support for OpenIDEA: a port of the Bazel IntelliJ plugin's Java sync
//! (bazelbuild/intellij, ijwb branch) that hands the project to jdtls as an Eclipse project.

pub mod aspect;
pub mod eclipse;
pub mod ideinfo;
pub mod import_roots;
pub mod model;
pub mod projectview;
pub mod query;
pub mod settings;
pub mod textproto;
pub mod types;
pub mod wire;
