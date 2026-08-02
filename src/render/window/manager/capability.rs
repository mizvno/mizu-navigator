//! `execute_tab_capability_action`: dispatches a declarative `RuntimeAction`
//! against a tab's capability policy, recording every network-visible
//! dispatch (and policy block) in that tab's inspector log.

use crate::network::RuntimeAction;

use super::types::{TabState, WindowCtx};

pub(crate) fn execute_tab_capability_action(
    tab: &mut TabState,
    ctx: &WindowCtx<'_>,
    action: RuntimeAction,
) {
    use crate::render::inspector::log::NetOutcome;
    use crate::render::security::CapabilityOutcome;

    // Describe network-visible actions before the action is moved.
    let described: Option<(String, String, Option<String>)> = match &action {
        RuntimeAction::ResolvedCall {
            method,
            url,
            target_variable,
            ..
        } => Some((
            method.clone(),
            url.clone(),
            Some(target_variable.0.to_string()),
        )),
        RuntimeAction::StoreLocal { key, .. } => Some(("STORE".to_string(), key.clone(), None)),
        RuntimeAction::DownloadMedia { url } => Some(("MEDIA".to_string(), url.clone(), None)),
        _ => None,
    };

    let outcome = crate::render::security::execute_capability_action(
        &mut tab.store,
        ctx.network_tx,
        ctx.logic_tx,
        tab.id,
        // The origin of record — never the URL-bar text, which the user may
        // be editing and which a stalled navigation must not be able to move.
        &tab.chrome_state.committed_url,
        &mut tab.capability_policy,
        action,
    );

    if let Some((verb, target, correlation)) = described {
        match outcome {
            CapabilityOutcome::Blocked(reason) => {
                tab.inspector_log.push_net_blocked(&verb, &target, reason);
            }
            CapabilityOutcome::Dispatched => {
                if verb == "STORE" {
                    // Fire-and-forget: no completion message flows back.
                    tab.inspector_log
                        .push_net_done(&verb, &target, NetOutcome::Ok);
                } else {
                    tab.inspector_log
                        .push_net_start(&verb, &target, correlation);
                }
            }
        }
    }
}
