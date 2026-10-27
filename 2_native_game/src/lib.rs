//! ROM-driven recompiler (4_docs/RECOMP_PLAN.md): decode ARM/Thumb, lift to Rust, run with a DS
//! service layer. Contains no game code or data.
pub mod arm;
pub mod arm7;
pub mod bios;
pub mod boot;
pub mod card;
pub mod dispatch;
pub mod ds_mem;
pub mod gfx2d;
pub mod gx;
/// Reference interpreter: tests, differential checks and diagnostics only; never in a shipped game.
#[cfg(feature = "interpreter")]
pub mod interp;
pub mod ir;
pub mod input;
pub mod input_bus;
pub mod lift;
pub mod machine;
pub mod mem;
pub mod native;
pub mod peripherals;
pub mod raise;
pub mod raise_emit;
pub mod raster;
pub mod residency;
pub mod rt;
pub mod shadow;
pub mod signature;
pub mod spu;
pub mod texture;
pub mod thumb;
