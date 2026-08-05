//! Tests for the Phase 3 capability-broker validation gate.

use super::{ActionOrigin, EventAgency, authorize_action};
use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value};
use crate::network::{RuntimeAction, UiEvent};
use crate::parser::{EndpointKind, UrlEndpoint, UrlRegistry};
use mizu_core::parser::logic::{NetworkMethod, PayloadFormat};

fn registry_with(alias: Symbol, endpoint: UrlEndpoint) -> UrlRegistry {
    let mut reg = UrlRegistry::default();
    reg.insert(alias, endpoint);
    reg
}

fn api_call(endpoint_symbol: u32) -> RuntimeAction {
    RuntimeAction::NetworkCall {
        method: NetworkMethod::Get,
        endpoint_symbol,
        payload: None,
        path_param: None,
        target_variable: Symbol(999),
        format: PayloadFormat::Json,
        headers: Vec::new(),
    }
}

#[test]
fn trusted_thread_actions_pass_through_unchanged() {
    let registry = UrlRegistry::default();
    let action = RuntimeAction::ResolvedCall {
        method: "GET".to_string(),
        url: "mizu://evil.example/whatever".to_string(),
        payload: None,
        target_variable: Symbol(1),
        format: PayloadFormat::Json,
        headers: Vec::new(),
    };
    let result = authorize_action(
        action,
        ActionOrigin::TrustedThread,
        "example.com",
        &registry,
        EventAgency::DocumentDriven,
    );
    match result.unwrap() {
        RuntimeAction::ResolvedCall { url, .. } => {
            assert_eq!(url, "mizu://evil.example/whatever");
        }
        other => panic!("expected ResolvedCall to pass through unchanged, got {other:?}"),
    }
}

#[test]
fn untrusted_worker_resolved_call_is_always_rejected() {
    let registry = UrlRegistry::default();
    let action = RuntimeAction::ResolvedCall {
        method: "GET".to_string(),
        url: "mizu://attacker.example/steal".to_string(),
        payload: None,
        target_variable: Symbol(1),
        format: PayloadFormat::Json,
        headers: Vec::new(),
    };
    let result = authorize_action(
        action,
        ActionOrigin::SandboxedIpcWorker,
        "example.com",
        &registry,
        EventAgency::DocumentDriven,
    );
    assert!(matches!(result, Err(MizuError::SecurityViolation(_))));
}

#[test]
fn untrusted_worker_download_media_is_always_rejected() {
    let registry = UrlRegistry::default();
    let action = RuntimeAction::DownloadMedia {
        url: "mizu://attacker.example/payload.bin".to_string(),
    };
    let result = authorize_action(
        action,
        ActionOrigin::SandboxedIpcWorker,
        "example.com",
        &registry,
        EventAgency::DocumentDriven,
    );
    assert!(matches!(result, Err(MizuError::SecurityViolation(_))));
}

#[test]
fn untrusted_worker_network_call_resolves_against_broker_registry() {
    let alias = Symbol(7);
    let registry = registry_with(
        alias,
        UrlEndpoint {
            kind: EndpointKind::Api,
            raw_target: "/users/{id}".to_string(),
        },
    );
    let mut action = api_call(7);
    if let RuntimeAction::NetworkCall { path_param, .. } = &mut action {
        *path_param = Some("42".to_string());
    }

    let result = authorize_action(
        action,
        ActionOrigin::SandboxedIpcWorker,
        "example.com",
        &registry,
        EventAgency::DocumentDriven,
    )
    .expect("known alias must resolve");

    match result {
        RuntimeAction::ResolvedCall { url, .. } => {
            assert_eq!(url, "mizu://example.com/users/42");
        }
        other => panic!("expected ResolvedCall, got {other:?}"),
    }
}

#[test]
fn untrusted_worker_network_call_with_unknown_alias_is_rejected() {
    let registry = UrlRegistry::default();
    let result = authorize_action(
        api_call(999),
        ActionOrigin::SandboxedIpcWorker,
        "example.com",
        &registry,
        EventAgency::DocumentDriven,
    );
    assert!(matches!(result, Err(MizuError::SecurityViolation(_))));
}

#[test]
fn untrusted_worker_download_alias_resolves_against_broker_registry() {
    let alias = Symbol(3);
    let registry = registry_with(
        alias,
        UrlEndpoint {
            kind: EndpointKind::Media,
            raw_target: "mizu://example.com/assets/logo.png".to_string(),
        },
    );
    let result = authorize_action(
        RuntimeAction::DownloadAlias { endpoint_symbol: 3 },
        ActionOrigin::SandboxedIpcWorker,
        "example.com",
        &registry,
        EventAgency::DocumentDriven,
    )
    .expect("known media alias must resolve");

    match result {
        RuntimeAction::DownloadMedia { url } => {
            assert_eq!(url, "mizu://example.com/assets/logo.png");
        }
        other => panic!("expected DownloadMedia, got {other:?}"),
    }
}

#[test]
fn navigate_requires_a_click_or_submit_form_gesture() {
    let registry = UrlRegistry::default();

    // A timer-triggered response must not be able to navigate.
    let blocked = authorize_action(
        RuntimeAction::Navigate {
            url: "mizu://example.com/next".to_string(),
        },
        ActionOrigin::SandboxedIpcWorker,
        "example.com",
        &registry,
        EventAgency::DocumentDriven,
    );
    assert!(matches!(blocked, Err(MizuError::SecurityViolation(_))));

    // A genuine click must be allowed through.
    let allowed = authorize_action(
        RuntimeAction::Navigate {
            url: "mizu://example.com/next".to_string(),
        },
        ActionOrigin::SandboxedIpcWorker,
        "example.com",
        &registry,
        EventAgency::UserGesture,
    );
    assert!(allowed.is_ok());

    // A form submission must also be allowed through.
    let allowed_submit = authorize_action(
        RuntimeAction::Navigate {
            url: "mizu://example.com/next".to_string(),
        },
        ActionOrigin::SandboxedIpcWorker,
        "example.com",
        &registry,
        // Derived from the event rather than written literally, so this stays
        // honest if `SubmitForm` ever stops counting as a gesture.
        EventAgency::of(&UiEvent::SubmitForm {
            submitter_node_id: 2,
            fields: Default::default(),
        }),
    );
    assert!(allowed_submit.is_ok());
}

/// The agency derivation itself, since every other test now depends on it.
#[test]
fn agency_is_derived_from_the_event_variant() {
    assert_eq!(
        EventAgency::of(&UiEvent::Click { node_id: 0 }),
        EventAgency::UserGesture
    );
    assert_eq!(
        EventAgency::of(&UiEvent::SubmitForm {
            submitter_node_id: 0,
            fields: Default::default(),
        }),
        EventAgency::UserGesture
    );
    for document_driven in [
        UiEvent::RootTimer { index: 0 },
        UiEvent::UpdateVariable {
            name: "x".to_string(),
            value: Value::Null,
        },
        UiEvent::CloseTab,
    ] {
        assert_eq!(
            EventAgency::of(&document_driven),
            EventAgency::DocumentDriven,
            "{document_driven:?} must not carry user agency"
        );
    }
}

#[test]
fn untrusted_worker_store_local_passes_through_unchanged() {
    let registry = UrlRegistry::default();
    let action = RuntimeAction::StoreLocal {
        key: "theme".to_string(),
        value: Value::String("dark".into()),
    };
    let result = authorize_action(
        action,
        ActionOrigin::SandboxedIpcWorker,
        "example.com",
        &registry,
        EventAgency::DocumentDriven,
    );
    match result.unwrap() {
        RuntimeAction::StoreLocal { key, value } => {
            assert_eq!(key, "theme");
            assert!(matches!(value, Value::String(s) if &*s == "dark"));
        }
        other => panic!("expected StoreLocal to pass through unchanged, got {other:?}"),
    }
}
