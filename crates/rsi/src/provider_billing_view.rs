//! TUI thread-local mirror of the daemon's effective billing modes (#3533).
//! Rendering helpers have no App; the TUI loop is single-threaded.
use std::cell::RefCell;
use std::collections::BTreeMap;
thread_local! { static MODES: RefCell<BTreeMap<String,String>> = const { RefCell::new(BTreeMap::new()) }; }
pub fn sync(config: &serde_json::Value) {
    if let Some(modes) = config
        .get("billing_modes")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        MODES.with(|view| *view.borrow_mut() = modes);
    }
}
pub fn is_metered(provider: &str, model: Option<&str>) -> bool {
    MODES.with(|view| {
        rsi_common::provider_billing::resolve(
            provider,
            model,
            view.borrow()
                .get(&provider.to_ascii_lowercase())
                .map(String::as_str),
            None,
        )
        .is_metered()
    })
}
pub fn session_is_metered(session: &rsi_common::types::Session) -> bool {
    is_metered(&format!("{:?}", session.provider), session.model.as_deref())
}

/// Restore the thread-local view when a billing-sensitive UI test ends.
#[cfg(test)]
pub(crate) fn metered_mode_for_test(provider: &str) -> impl Drop {
    struct Restore {
        provider: String,
        previous: Option<String>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            MODES.with(|view| {
                let mut modes = view.borrow_mut();
                if let Some(previous) = self.previous.take() {
                    modes.insert(self.provider.clone(), previous);
                } else {
                    modes.remove(&self.provider);
                }
            });
        }
    }
    let provider = provider.to_ascii_lowercase();
    let previous = MODES.with(|view| {
        view.borrow_mut()
            .insert(provider.clone(), "metered_api".into())
    });
    Restore { provider, previous }
}
