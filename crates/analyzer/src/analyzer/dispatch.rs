//! Dispatch resolution at call sites.
//!
//! Phase 1 placeholder (`resolve_call_site_stub`) returns Mixed receiver for
//! everything. Phase 2 adds `resolve()` which converts a CallSiteEvent into a
//! CallSite using the Type → ReceiverType bridge. Task 2.11 will switch all
//! call sites to the new `resolve()`.

use crate::opacity::{CallSite, ReceiverType};
use crate::types::walker::CallSiteEvent;
use crate::types::type_to_receiver_type;

/// Build a `CallSite` from minimal information available without type inference.
///
/// Phase 1 returns `ReceiverType::Mixed` for everything because we don't yet have
/// a type tracker. The opacity layer will treat all such calls as opaque, which
/// means the analyzer marks call site lines but doesn't recurse into callees.
pub fn resolve_call_site_stub(
    callee_class: Option<String>,
    callee_method: String,
    callee_file: Option<std::path::PathBuf>,
) -> CallSite {
    CallSite {
        callee_class,
        callee_method,
        callee_file,
        receiver_type: ReceiverType::Mixed,
    }
}

/// Convert a CallSiteEvent (from the walker) into the CallSite the opacity layer needs.
pub fn resolve(event: &CallSiteEvent, enclosing_class: Option<&str>) -> CallSite {
    CallSite {
        callee_class: event.callee_class.clone(),
        callee_method: event.method_name.clone(),
        callee_file: event.callee_file.clone(),
        receiver_type: type_to_receiver_type(&event.receiver, enclosing_class),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_yields_mixed_receiver() {
        let cs = resolve_call_site_stub(Some("Foo".into()), "bar".into(), None);
        assert_eq!(cs.receiver_type, ReceiverType::Mixed);
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::opacity::ReceiverType;
    use crate::types::Type;

    #[test]
    fn resolve_concrete() {
        let ev = CallSiteEvent {
            line: 10,
            receiver: Type::Class("Money".into()),
            method_name: "add".into(),
            callee_class: Some("Money".into()),
            callee_file: Some(std::path::PathBuf::from("/path/to/Money.php")),
        };
        let cs = resolve(&ev, Some("MoneyTest"));
        assert_eq!(cs.receiver_type, ReceiverType::Concrete("Money".into()));
        assert_eq!(cs.callee_method, "add");
        assert_eq!(cs.callee_class.as_deref(), Some("Money"));
    }

    #[test]
    fn resolve_this() {
        let ev = CallSiteEvent {
            line: 5,
            receiver: Type::This,
            method_name: "helper".into(),
            callee_class: None,
            callee_file: None,
        };
        let cs = resolve(&ev, Some("MoneyTest"));
        assert_eq!(cs.receiver_type, ReceiverType::Concrete("MoneyTest".into()));
    }

    #[test]
    fn resolve_mock() {
        let ev = CallSiteEvent {
            line: 7,
            receiver: Type::Mock("UserRepository".into()),
            method_name: "find".into(),
            callee_class: None,
            callee_file: None,
        };
        let cs = resolve(&ev, Some("UserServiceTest"));
        assert_eq!(cs.receiver_type, ReceiverType::Mock);
    }
}
