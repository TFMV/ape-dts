use strum::{AsStaticStr, Display, EnumString};

#[derive(Debug, Clone, PartialEq, Display, EnumString, AsStaticStr)]
pub enum RowType {
    #[strum(serialize = "insert")]
    Insert,
    #[strum(serialize = "update")]
    Update,
    #[strum(serialize = "delete")]
    Delete,
}
