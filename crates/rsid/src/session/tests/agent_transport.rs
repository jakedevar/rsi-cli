//! Issuance and privacy gates for the daemon-owned opaque launch witness.

use super::*;
use syn::visit::{self, Visit};

#[derive(Default)]
struct WitnessScanner {
    registry: bool,
    registry_file: bool,
    test_source: bool,
    function: String,
    definitions: usize,
    constructors: usize,
    remints: usize,
    violations: Vec<&'static str>,
}

fn named(path: &syn::Path, name: &str) -> bool {
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == name)
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Path>()
                .is_ok_and(|path| path.is_ident("test"))
    })
}

impl<'ast> Visit<'ast> for WitnessScanner {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if test_only(&item.attrs) {
            return;
        }
        let previous = self.registry;
        self.registry = self.registry_file && item.ident == "agent_token_registry";
        visit::visit_item_mod(self, item);
        self.registry = previous;
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
        self.function = previous;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        visit::visit_impl_item_fn(self, item);
        self.function = previous;
    }

    fn visit_expr_method_call(&mut self, expr: &'ast syn::ExprMethodCall) {
        if expr.method == "remint" && !self.test_source {
            self.remints += 1;
            if !self.registry_file || self.function != "remint_agent_token_with_witness" {
                self.violations
                    .push("production remint outside the issuance seam");
            }
        }
        visit::visit_expr_method_call(self, expr);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if item.ident == "AgentTransportWitness" {
            self.definitions += 1;
            if !self.registry
                || item
                    .fields
                    .iter()
                    .any(|field| !matches!(field.vis, syn::Visibility::Inherited))
            {
                self.violations
                    .push("witness definition/fields escape the registry");
            }
            for attr in &item.attrs {
                if attr.path().is_ident("derive") {
                    let derives = attr.parse_args_with(
                        syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
                    ).expect("parse derives");
                    for path in derives {
                        if !["Debug", "Clone", "Copy", "PartialEq", "Eq"]
                            .iter()
                            .any(|name| named(&path, name))
                        {
                            self.violations
                                .push("witness derives a construction or serialization route");
                        }
                    }
                } else if !attr.path().is_ident("doc") {
                    // Doc comments are inert; any other attribute (serde, cfg_attr, ...)
                    // could add a construction or serialization route.
                    self.violations.push("unreviewed witness attribute");
                }
            }
        }
        visit::visit_item_struct(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if let syn::Type::Path(ty) = item.self_ty.as_ref()
            && named(&ty.path, "AgentTransportWitness")
        {
            // The witness deliberately has no methods or trait constructors.
            self.violations
                .push("witness gains an impl/constructor route");
        }
        visit::visit_item_impl(self, item);
    }

    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        if named(&expr.path, "AgentTransportWitness") {
            self.constructors += 1;
            if !self.registry {
                self.violations.push("witness constructor outside registry");
            }
        }
        visit::visit_expr_struct(self, expr);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn agent_transport_witness_issuance_is_confined_to_registry() {
    let root = std::path::Path::new(rsi_common::manifest_dir!())
        .parent()
        .unwrap()
        .to_path_buf();
    let mut scanner = WitnessScanner::default();
    for package in ["rsid", "rsid-store", "rsid-core", "rsi-common"] {
        for entry in walkdir::WalkDir::new(root.join(package).join("src")) {
            let entry = entry.expect("walk source");
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "rs")
            {
                let relative = entry.path().strip_prefix(&root).unwrap();
                scanner.registry_file = relative == std::path::Path::new("rsid/src/session/mod.rs");
                scanner.test_source = relative
                    .components()
                    .any(|component| component.as_os_str() == "tests")
                    || entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name == "tests.rs" || name.ends_with("_tests.rs"));
                let source = std::fs::read_to_string(entry.path()).expect("read source");
                scanner.visit_file(&syn::parse_file(&source).expect("parse source"));
            }
        }
    }
    assert_eq!(scanner.definitions, 1);
    assert_eq!(scanner.constructors, 1);
    assert!(scanner.violations.is_empty(), "{:?}", scanner.violations);

    assert_eq!(scanner.remints, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn agent_token_registry_debug_redacts_both_indexes() {
    let mut registry = super::super::AgentTokenRegistry::default();
    let marker = crate::test_support::agent_token(&format!("synthetic-bearer-{}", Uuid::new_v4()));
    registry.insert(marker.clone(), Uuid::new_v4());
    let diagnostic = format!("{registry:?}");
    assert!(
        !diagnostic.contains(&marker),
        "diagnostics exposed a synthetic credential"
    );
    assert!(diagnostic.contains("<redacted>"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn agent_transport_witness_scanner_refuses_constructor_and_default_routes() {
    for source in [
        "fn forge() { AgentTransportWitness { incarnation_id: id }; }",
        "#[derive(Default)] struct AgentTransportWitness { incarnation_id: Uuid }",
        "#[derive(serde::Deserialize)] struct AgentTransportWitness { incarnation_id: Uuid }",
        "impl From<Uuid> for AgentTransportWitness {}",
        "fn forge(registry: &mut Registry) { registry.remint(token, session); }",
    ] {
        let mut scanner = WitnessScanner {
            registry_file: true,
            ..Default::default()
        };
        scanner.visit_file(&syn::parse_file(source).unwrap());
        assert!(
            !scanner.violations.is_empty(),
            "scanner must reject an authority escape"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn agent_transport_stale_cleanup_preserves_same_token_replacement() {
    let session_id = Uuid::new_v4();
    let marker = crate::test_support::agent_token(&format!("synthetic-bearer-{}", Uuid::new_v4()));
    let mut registry = super::super::AgentTokenRegistry::default();
    registry.insert(marker.clone(), session_id);
    let stale = registry.binding_for_session(session_id).unwrap();
    registry.insert(marker.clone(), session_id);
    let current = registry.binding_for_session(session_id).unwrap();
    assert!(!registry.revoke_session_if_binding(session_id, stale));
    assert!(registry.binding_matches(session_id, current));
    assert_eq!(registry.get(&marker), Some(&session_id));
    assert!(registry.revoke_session_if_binding(session_id, current));
    assert_eq!(registry.binding_for_session(session_id), None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn agent_transport_digest_uses_exact_domain_and_refuses_verifier_replay() {
    let session_id = Uuid::new_v4();
    let token = "a".repeat(64);
    let mut registry = super::super::AgentTokenRegistry::default();
    registry.insert(token.clone(), session_id);
    let verifier = hex::encode(registry.verifier_for_session(session_id).unwrap());
    assert_eq!(
        verifier,
        "37ed494685f45e17a5222ab3b3e476fc362722e6c38f977cbda09ec531b345ce"
    );
    assert_eq!(registry.get(&token), Some(&session_id));
    assert_eq!(registry.get(&verifier), None, "verifier is not a bearer");
    assert_eq!(registry.get(&format!("sha256:{verifier}")), None);
    let diagnostic = format!("{registry:?}");
    assert!(!diagnostic.contains(&token));
    assert!(!diagnostic.contains(&verifier));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn agent_transport_digest_refuses_malformed_oversized_and_unknown_bearers() {
    let (manager, _directory) = manager();
    let session_id = Uuid::new_v4();
    let token = manager.remint_session_token(session_id).await;
    let exact = token.expose_for_transport();
    assert_eq!(manager.resolve_agent_token(exact).await, Some(session_id));
    let other = manager.remint_session_token(Uuid::new_v4()).await;
    let unknown = super::super::generate_session_token();
    for rejected in [
        String::new(),
        exact[..63].to_string(),
        format!("{exact}0"),
        exact.to_ascii_uppercase(),
        format!(" {exact}"),
        format!("{exact}\n"),
        "g".repeat(64),
        "é".repeat(32),
        "a".repeat(1024 * 1024),
        unknown.expose_for_transport().to_string(),
    ] {
        assert!(manager.resolve_agent_token(&rejected).await.is_none());
    }
    assert_eq!(manager.resolve_agent_token(exact).await, Some(session_id));
    assert!(
        manager
            .resolve_agent_token(other.expose_for_transport())
            .await
            .is_some()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn agent_transport_production_registry_retains_only_verifiers_and_witnesses() {
    let file = syn::parse_file(include_str!("../mod.rs")).unwrap();
    let module = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Mod(module) if module.ident == "agent_token_registry" => Some(module),
            _ => None,
        })
        .unwrap();
    let items = &module.content.as_ref().unwrap().1;
    assert!(registry_retains_only_non_secret_fields(items));
    let token_readback = items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(item) => Some(&item.items),
            _ => None,
        })
        .flatten()
        .find_map(|item| match item {
            syn::ImplItem::Fn(item) if item.sig.ident == "token_for_session" => Some(item),
            _ => None,
        })
        .unwrap();
    assert!(test_only(&token_readback.attrs));
}

// Check types as well as names, recursively through every retained registry
// kind and the copied caller. The Boolean is provenance only; changing it (or
// any witness/index field) to a bearer container must fail this gate.
fn registry_retains_only_non_secret_fields(items: &[syn::Item]) -> bool {
    let expected: &[(&str, &[(&str, &str)])] = &[
        ("AgentTransportVerifier", &[("", "[u8;32]")]),
        ("AgentTransportWitness", &[("incarnation_id", "Uuid")]),
        (
            "BoundAgentCaller",
            &[
                ("session_id", "Uuid"),
                ("witness", "AgentTransportWitness"),
                ("fresh_launch", "bool"),
            ],
        ),
        (
            "AgentTokenBinding",
            &[
                ("verifier", "AgentTransportVerifier"),
                ("witness", "AgentTransportWitness"),
                ("fresh_launch", "bool"),
            ],
        ),
        (
            "AgentTokenRegistry",
            &[
                ("by_verifier", "HashMap<AgentTransportVerifier,Uuid>"),
                ("by_session", "HashMap<Uuid,AgentTokenBinding>"),
            ],
        ),
    ];
    let structs: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Struct(item) if !test_only(&item.attrs) => Some(item),
            _ => None,
        })
        .collect();
    structs.len() == expected.len()
        && expected.iter().all(|(name, fields)| {
            let Some(item) = structs.iter().find(|item| item.ident == *name) else {
                return false;
            };
            let production: Vec<_> = item
                .fields
                .iter()
                .filter(|field| !test_only(&field.attrs))
                .collect();
            production.len() == fields.len()
                && production.iter().zip(*fields).all(|(field, (name, ty))| {
                    field
                        .ident
                        .as_ref()
                        .map_or(name.is_empty(), |ident| ident == name)
                        && retained_type(&field.ty).as_deref() == Some(*ty)
                })
        })
}

// Only the simple owned types used by this registry are accepted. References,
// aliases to unlisted types, macros, and other opaque containers fail closed.
fn retained_type(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(ty)
            if ty.qself.is_none()
                && ty.path.leading_colon.is_none()
                && ty.path.segments.len() == 1 =>
        {
            let segment = ty.path.segments.first()?;
            let name = segment.ident.to_string();
            match &segment.arguments {
                syn::PathArguments::None => Some(name),
                syn::PathArguments::AngleBracketed(args) if args.colon2_token.is_none() => {
                    let types: Option<Vec<_>> = args
                        .args
                        .iter()
                        .map(|arg| match arg {
                            syn::GenericArgument::Type(ty) => retained_type(ty),
                            _ => None,
                        })
                        .collect();
                    Some(format!("{name}<{}>", types?.join(",")))
                }
                _ => None,
            }
        }
        syn::Type::Array(array) => {
            let syn::Expr::Lit(length) = &array.len else {
                return None;
            };
            let syn::Lit::Int(length) = &length.lit else {
                return None;
            };
            Some(format!(
                "[{};{}]",
                retained_type(&array.elem)?,
                length.base10_digits()
            ))
        }
        _ => None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn agent_transport_registry_gate_refuses_secret_fields_in_every_retained_kind() {
    let source = include_str!("../mod.rs");
    for (original, secret) in [
        (
            "AgentTransportVerifier([u8; 32])",
            "AgentTransportVerifier(String)",
        ),
        (
            "incarnation_id: Uuid,",
            "incarnation_id: Uuid, bearer: String,",
        ),
        (
            "session_id: Uuid,\n        witness:",
            "session_id: String,\n        witness:",
        ),
        ("fresh_launch: bool,", "fresh_launch: String,"),
        (
            "verifier: AgentTransportVerifier,",
            "verifier: rsi_common::rpc::AgentTransportBearer,",
        ),
        (
            "HashMap<AgentTransportVerifier, Uuid>",
            "HashMap<String, Uuid>",
        ),
        ("HashMap<Uuid, AgentTokenBinding>", "HashMap<Uuid, String>"),
    ] {
        assert!(source.contains(original), "mutation must change the source");
        let file = syn::parse_file(&source.replace(original, secret)).unwrap();
        let items = file
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Mod(module) if module.ident == "agent_token_registry" => {
                    Some(&module.content.as_ref().unwrap().1)
                }
                _ => None,
            })
            .unwrap();
        assert!(
            !registry_retains_only_non_secret_fields(items),
            "accepted secret mutation: {secret}"
        );
    }
}
