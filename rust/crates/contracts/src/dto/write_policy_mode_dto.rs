use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WritePolicyModeDto {
    #[serde(rename = "auto_readonly_confirm_writes")]
    AutoReadonlyConfirmWrites,
    #[serde(rename = "confirm_all_writes")]
    ConfirmAllWrites,
    #[serde(rename = "auto_all")]
    AutoAll,
}
crate::impl_json_via_serde!(WritePolicyModeDto);
