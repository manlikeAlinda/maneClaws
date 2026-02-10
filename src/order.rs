use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct NewOrder<'a> {
    pub symbol: &'a str,
    pub side: &'a str,
    #[serde(rename = "type")]
    pub order_type: &'a str,
    pub quantity: String,
}

impl<'a> NewOrder<'a> {
    pub fn to_query_string(&self) -> String {
        // Binance expects: key=value&key=value...
        // Keep it stable and explicit
        format!(
            "symbol={}&side={}&type={}&quantity={}",
            self.symbol, self.side, self.order_type, self.quantity
        )
    }
}
