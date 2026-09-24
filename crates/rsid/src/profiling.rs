use std::sync::OnceLock;

static ENABLED: OnceLock<bool> = OnceLock::new();

#[inline]
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        // RSID_PROFILE / RSI_PROFILE take precedence; legacy MOTHERSHIPD_/MOTHERSHIP_/FLYWHL_
        // names are honored so existing workflows don't break.
        let keys = [
            "RSID_PROFILE",
            "RSI_PROFILE",
            "MOTHERSHIPD_PROFILE",
            "MOTHERSHIP_PROFILE",
            "FLYWHL_PROFILE",
        ];
        for key in keys {
            if let Ok(value) = std::env::var(key) {
                if !value.is_empty() && value != "0" {
                    return true;
                }
            }
        }
        false
    })
}
