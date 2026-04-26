use lixun_mutation::AnnHandle;

/// Approximate-nearest-neighbour handle backed by LanceDB. Empty
/// marker impl in the skeleton; query and ingest surfaces land in
/// WD-T5 / WD-T7 alongside `lixun-fusion`'s hybrid wiring.
pub struct LanceDbAnnHandle;

impl AnnHandle for LanceDbAnnHandle {}
