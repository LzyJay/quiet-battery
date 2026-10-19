//! Included in the executable so standalone binary copies carry license notices.
pub const TEXT: &str = concat!(
    include_str!("../NOTICE"),
    "\n\n",
    include_str!("../LICENSE"),
    "\n\n",
    include_str!("../THIRD_PARTY_NOTICES.txt")
);
