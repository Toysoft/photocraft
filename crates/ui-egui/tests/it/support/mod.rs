//! Helpers shared by the modules of this test binary.

pub mod layout_doc;

// GPU tests take `photocraft_testkit::gpu_lock()`: it is process-wide, so it serializes every
// GPU test of this binary across modules.
