//! The disassembler: one line per instruction, driven by the table.
//!
//! ```text
//!     0  push_i32 42
//!     5  get_field "length"
//!    10  jump_if_false -> 25
//!    15  get_loc loc1 (x)
//! ```
