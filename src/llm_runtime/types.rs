#[derive(Clone, Debug, Default)]
pub struct LlmResponse {
    pub raw: String,
    pub tab_id: Option<u32>,
    pub turn_id: Option<u64>,
}
