//! Shared ROM validation, ARM9 extraction and asset ripping (used by the builder and the recompiler).
pub mod controls;
pub mod extract;
pub mod rip;
pub mod rom;
pub mod settings;
pub mod dsfiles;
pub mod modding;
pub mod modpkg;

extern crate self as mkds_builder;
pub mod build;
