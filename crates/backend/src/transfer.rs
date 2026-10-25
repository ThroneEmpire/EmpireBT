/// High-level transfer lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    Paused,
    Downloading,
    Seeding,
    Complete,
}
