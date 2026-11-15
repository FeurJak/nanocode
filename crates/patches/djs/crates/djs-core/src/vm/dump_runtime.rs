//! The plain dump runtime (D2834, D2903): the format's fixed helpers, which every host
//! provides as members of `$djs` next to its hooks (`seed`, `resume`, `source`, `host`), so that
//! a dump does not carry them. They are the same code in every dump: a small standard
//! library of the format, not program content.
//!
//! [`HELPERS`] is the one definition. Hosts on djs get the object from
//! [`Vm::dump_runtime`] (the CLI, the difftest) or, with the text from
//! js/src/dump-runtime.js, [`Vm::dump_runtime_from`] (the wasm export `vm_dump_runtime` for
//! the Sandbox and pi's adapter, so that the shipped module does not carry the text); hosts
//! without djs (node) import js/src/dump-runtime.js, which [`module_text`] writes from the
//! same table (a test keeps the file current).
//!
//! Every helper is a plain function (or a builtin) that depends neither on `this` nor on
//! another member through the object (`FM` and `CC` call `L` lexically): `$djs` is a global for now and meant to become
//! imports, one named export per helper.
