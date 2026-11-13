use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalGrantRefDto {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "grantId")]
    pub grant_id: String,
    #[serde(rename = "digest")]
    pub digest: String,
}
crate::impl_json_deserialize!(ApprovalGrantRefDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        session_id: crate::required_field(&mut object, "sessionId")?,
        grant_id: crate::required_field(&mut object, "grantId")?,
        digest: crate::required_field(&mut object, "digest")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
