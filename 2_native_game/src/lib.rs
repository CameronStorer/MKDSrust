//! ROM-driven recompiler (4_docs/RECOMP_PLAN.md): decode ARM/Thumb, lift to Rust, run with a DS
//! service layer. Contains no game code or data.
pub mod arm;
pub mod save;
pub mod arm7;
pub mod bios;
pub mod boot;
pub mod card;
pub mod dispatch;
pub mod ds_mem;
pub mod gfx2d;
pub mod gx;
pub mod hle7;
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
/// Block 5.3: OS time and interrupts on the host (`MKDS_HW_TIME=native`).
pub mod os_hle;
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

/// Set player hardware defaults before constructing hardware or starting threads.
pub fn player_defaults() {
    for key in ["MKDS_HW_FS", "MKDS_HW_ARM7", "MKDS_HW_TIME", "MKDS_HW_SND"] {
        if std::env::var_os(key).is_none() {
            unsafe { std::env::set_var(key, "native"); }
        }
    }
}
