use crate::{
    action::{Action, ActionWrapper},
    context::Context,
    nucleus::{
        actions::get_entry::get_entry_from_agent_chain,
        ribosome::{self, WasmCallData},
        ZomeFnCall, ZomeFnResult,
    },
};
use holochain_core_types::{
    dna::{capabilities::CapabilityRequest, wasm::DnaWasm},
    entry::{
        cap_entries::{CapTokenGrant, CapabilityType},
        Entry,
    },
    error::HolochainError,
    signature::{Provenance, Signature},
    ugly::lax_send_sync,
};

use holochain_persistence_api::cas::content::{Address, AddressableContent};

use holochain_json_api::json::JsonString;

use holochain_dpki::utils::Verify;

use base64;
use futures::{future::Future, task::Poll};
use holochain_wasm_utils::api_serialization::crypto::CryptoMethod;
use snowflake::ProcessUniqueId;
use std::{pin::Pin, sync::Arc, thread};

#[derive(Clone, Debug, PartialEq, Hash, Serialize)]
pub struct ExecuteZomeFnResponse {
    call: ZomeFnCall,
    result: ZomeFnResult,
}

impl ExecuteZomeFnResponse {
    pub fn new(call: ZomeFnCall, result: Result<JsonString, HolochainError>) -> Self {
        ExecuteZomeFnResponse { call, result }
    }

    /// read only access to call
    pub fn call(&self) -> ZomeFnCall {
        self.call.clone()
    }

    /// read only access to result
    pub fn result(&self) -> Result<JsonString, HolochainError> {
        self.result.clone()
    }
}

/// Execution of zome calls
/// This function is kicking off the execution of a given zome function with given parameters.
/// It dispatches two actions:
/// * `SignalZomeFunctionCall`: after passing checks and before actually starting the Ribosome,
/// * `ReturnZomeFunctionResult`: asynchronously after execution of the Ribosome has completed.
///
/// It is doing pre-checks (such as the capability check) synchronously but then spawns a new
/// thread to run the Ribosome in.
///
/// Being an async function, it returns a future that is polling the instance's State until
/// the call result gets added there through the `RetunrZomeFunctionResult` action.
///
/// Use Context::block_on to wait for the call result.
pub async fn call_zome_function(
    zome_call: ZomeFnCall,
    context: Arc<Context>,
) -> Result<JsonString, HolochainError> {
    log_debug!(context,
        "actions/call_zome_fn: Validating call: {:?}",
        zome_call
    );

    // 1. Validate the call (a number of things could go wrong)
    validate_call(context.clone(), &zome_call)?;

    log_debug!(context,
        "actions/call_zome_fn: executing call: {:?}",
        zome_call
    );

    // Clone context and call data for the Ribosome thread
    let context_clone = context.clone();
    let zome_call_clone = zome_call.clone();

    // Signal (currently mainly to the nodejs_waiter) that we are about to start a zome function:
    context
        .action_channel()
        .send(ActionWrapper::new(Action::SignalZomeFunctionCall(
            zome_call.clone(),
        )))
        .expect("action channel to be open");

    thread::Builder::new()
        .name(format!(
            "call_zome_function/{}",
            ProcessUniqueId::new().to_string()
        ))
        .spawn(move || {
            // Have Ribosome spin up DNA and call the zome function
            let call_result = ribosome::run_dna(
                Some(zome_call_clone.clone().parameters.to_bytes()),
                WasmCallData::new_zome_call(context_clone.clone(), zome_call_clone.clone()),
            );
            log_debug!(context_clone, "actions/call_zome_fn: got call_result from ribosome::run_dna.");
            // Construct response
            let response = ExecuteZomeFnResponse::new(zome_call_clone, call_result);
            // Send ReturnZomeFunctionResult Action
            log_debug!(context_clone, "actions/call_zome_fn: sending ReturnZomeFunctionResult action.");
            lax_send_sync(
                context_clone.action_channel().clone(),
                ActionWrapper::new(Action::ReturnZomeFunctionResult(response)),
                "call_zome_function",
            );
            log_debug!(context_clone, "actions/call_zome_fn: sent ReturnZomeFunctionResult action.");
        })
        .expect("Could not spawn thread for call_zome_function");

    log_debug!(context,
        "actions/call_zome_fn: awaiting for \
         future call result of {:?}",
        zome_call
    );

    await!(CallResultFuture {
        context: context.clone(),
        zome_call,
    })
}

/// validates that a given zome function call specifies a correct zome function and capability grant
pub fn validate_call(
    context: Arc<Context>,
    fn_call: &ZomeFnCall,
) -> Result<(String, DnaWasm), HolochainError> {
    // make sure the dna, zome and function exists and return pretty errors if they don't
    let (dna_name, code) = {
        let state = context.state().ok_or(HolochainError::ErrorGeneric(
            "Context not initialized".to_string(),
        ))?;

        let nucleus_state = state.nucleus();
        let dna = nucleus_state
            .dna()
            .ok_or_else(|| HolochainError::DnaMissing)?;
        let zome = dna
            .get_zome(&fn_call.zome_name)
            .map_err(|e| HolochainError::Dna(e))?;
        let _ = dna
            .get_function_with_zome_name(&fn_call.zome_name, &fn_call.fn_name)
            .map_err(|e| HolochainError::Dna(e))?;
        (dna.name.clone(), zome.code.clone())
    };

    if check_capability(context.clone(), fn_call)
        || (is_token_the_agent(context.clone(), &fn_call.cap)
            && verify_call_sig(
                &fn_call.cap.provenance,
                &fn_call.fn_name,
                fn_call.parameters.clone(),
            ))
    {
        Ok((dna_name, code))
    } else {
        Err(HolochainError::CapabilityCheckFailed)
    }
}

fn is_token_the_agent(context: Arc<Context>, request: &CapabilityRequest) -> bool {
    context.agent_id.pub_sign_key == request.cap_token.to_string()
}

fn get_grant(context: &Arc<Context>, address: &Address) -> Option<CapTokenGrant> {
    match get_entry_from_agent_chain(context, address).ok()?? {
        Entry::CapTokenGrant(grant) => Some(grant),
        _ => None,
    }
}

/// checks to see if a given function call is allowable according to the capabilities
/// that have been registered to callers by looking for grants in the chain.
pub fn check_capability(context: Arc<Context>, fn_call: &ZomeFnCall) -> bool {
    let maybe_grant = get_grant(&context.clone(), &fn_call.cap_token());
    match maybe_grant {
        None => false,
        Some(grant) => verify_grant(context.clone(), &grant, fn_call),
    }
}

pub fn encode_call_data_for_signing<J: Into<JsonString>>(function: &str, parameters: J) -> String {
    base64::encode(&format!("{}:{}", function, parameters.into()))
}

// temporary function to create a mock signature of for a zome call cap request
fn make_call_sig<J: Into<JsonString>>(
    context: Arc<Context>,
    function: &str,
    parameters: J,
) -> Signature {
    let encode_call_data = encode_call_data_for_signing(function, parameters);
    Signature::from(
        context
            .conductor_api
            .execute(encode_call_data, CryptoMethod::Sign)
            .expect("signing should work"),
    )
}

// temporary function to verify a mock signature of for a zome call cap request
pub fn verify_call_sig<J: Into<JsonString>>(
    provenance: &Provenance,
    function: &str,
    parameters: J,
) -> bool {
    let what_was_signed = encode_call_data_for_signing(function, parameters);
    provenance.verify(what_was_signed).unwrap()
}

/// creates a capability request for a zome call by signing the function name and parameters
pub fn make_cap_request_for_call<J: Into<JsonString>>(
    callers_context: Arc<Context>,
    cap_token: Address,
    function: &str,
    parameters: J,
) -> CapabilityRequest {
    CapabilityRequest::new(
        cap_token,
        callers_context.agent_id.address(),
        make_call_sig(callers_context, function, parameters),
    )
}

/// verifies that this grant is valid for a given requester and token value
pub fn verify_grant(context: Arc<Context>, grant: &CapTokenGrant, fn_call: &ZomeFnCall) -> bool {
    let cap_functions = grant.functions();
    let maybe_zome_grants = cap_functions.get(&fn_call.zome_name);
    if maybe_zome_grants.is_none() {
        log_debug!(context,
            "actions/verify_grant: no grant for zome {:?} in grant {:?}",
            fn_call.zome_name, cap_functions
        );
        return false;
    }
    if !maybe_zome_grants.unwrap().contains(&fn_call.fn_name) {
        log_debug!(context,
            "actions/verify_grant: no grant for function {:?} in grant {:?}",
            fn_call.fn_name, maybe_zome_grants
        );
        return false;
    }

    if grant.token() != fn_call.cap_token() {
        log_debug!(context,
            "actions/verify_grant: grant token doesn't match: expecting {:?} got {:?}",
            grant.token(),
            fn_call.cap_token()
        );
        return false;
    }

    if !verify_call_sig(
        &fn_call.cap.provenance,
        &fn_call.fn_name,
        fn_call.parameters.clone(),
    ) {
        log_debug!(context, "actions/verify_grant: call signature did not match");
        return false;
    }

    match grant.cap_type() {
        CapabilityType::Public => true,
        CapabilityType::Transferable => true,
        CapabilityType::Assigned => {
            // unwraps are safe because type comes from the shape of
            // the assignee, and the from must some by the check above.
            if !grant
                .assignees()
                .unwrap()
                .contains(&fn_call.cap.provenance.source())
            {
                log_debug!(context, "actions/verify_grant: caller not one of the assignees");
                return false;
            }
            true
        }
    }
}

/// CallResultFuture resolves to an Result<JsonString, HolochainError>.
/// Tracks the nucleus State, waiting for a result to the given zome function call to appear.
pub struct CallResultFuture {
    context: Arc<Context>,
    zome_call: ZomeFnCall,
}

impl Future for CallResultFuture {
    type Output = Result<JsonString, HolochainError>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        if let Some(err) = self.context.action_channel_error("CallResultFuture") {
            return Poll::Ready(Err(err));
        }
        // With our own executor implementation in Context::block_on we actually
        // wouldn't need the waker since this executor is attached to the redux loop
        // and re-polls after every State mutation.
        // Leaving this in to be safe against running this future in another executor.
        cx.waker().clone().wake();

        if let Some(state) = self.context.state() {
            match state.nucleus().zome_call_result(&self.zome_call) {
                Some(result) => Poll::Ready(result),
                None => Poll::Pending,
            }
        } else {
            Poll::Ready(Err(HolochainError::ErrorGeneric(
                "State not initialized".to_string(),
            )))
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::{
        context::Context,
        instance::tests::*,
        nucleus::{actions::tests::test_dna, tests::*},
        workflows::author_entry::author_entry,
    };
    use holochain_core_types::{
        dna::capabilities::CapabilityRequest,
        entry::{
            cap_entries::{CapFunctions, CapTokenGrant, CapabilityType},
            Entry,
        },
        signature::Signature,
    };
    use holochain_persistence_api::cas::content::{Address, AddressableContent};

    #[test]
    fn test_agent_as_token() {
        let context = test_context("alice", None);
        let agent_token = context.agent_id.address();
        let cap_request =
            make_cap_request_for_call(context.clone(), agent_token.clone(), "test", "{}");
        assert!(is_token_the_agent(context.clone(), &cap_request));

        // bogus token should fail
        let cap_request = CapabilityRequest::new(
            Address::from("fake_token"),
            Address::from("someone"),
            Signature::fake(),
        );
        assert!(!is_token_the_agent(context, &cap_request));
    }

    #[test]
    fn test_call_signatures() {
        let context1 = test_context("alice", None);
        let context2 = test_context("bob", None);

        // only exact same call signed by the same person should verify
        let call_sig1 = make_call_sig(context1.clone(), "func", "{}");
        let provenance1 = Provenance::new(context1.agent_id.address(), call_sig1.clone());
        assert!(verify_call_sig(&provenance1, "func", "{}"));
        assert!(!verify_call_sig(&provenance1, "func1", "{}"));
        assert!(!verify_call_sig(&provenance1, "func", "{\"x\":1}"));

        let bad_provenance = Provenance::new(context2.agent_id.address(), call_sig1);

        assert!(!verify_call_sig(&bad_provenance, "func", "{}"));
    }

    #[test]
    fn test_make_cap_request_for_call() {
        let context = test_context("alice", None);
        let cap_request =
            make_cap_request_for_call(context.clone(), dummy_capability_token(), "some_fn", "{}");
        assert_eq!(cap_request.cap_token, dummy_capability_token());
        assert_eq!(
            cap_request.provenance.source().to_string(),
            context.agent_id.pub_sign_key
        );
        assert_eq!(
            cap_request.provenance.signature(),
            make_call_sig(context, "some_fn", "{}")
        );
    }

    #[test]
    fn test_get_grant() {
        let dna = test_dna();
        let (_instance, context) =
            test_instance_and_context(dna, None).expect("Could not initialize test instance");

        let mut cap_functions = CapFunctions::new();
        cap_functions.insert("test_zome".to_string(), vec![String::from("test")]);
        let grant = CapTokenGrant::create("foo", CapabilityType::Transferable, None, cap_functions)
            .unwrap();
        let grant_entry = Entry::CapTokenGrant(grant.clone());
        let grant_addr = context
            .block_on(author_entry(&grant_entry, None, &context, &vec![]))
            .unwrap()
            .address();
        let maybe_grant = get_grant(&context, &grant_addr);
        assert_eq!(maybe_grant, Some(grant));
    }

    #[test]
    fn test_verify_grant() {
        let context = test_context("alice", None);
        let context2 = test_context("bob", None);
        let test_address1 = context.agent_id.address();

        fn zome_call_valid(context: Arc<Context>, token: &Address) -> ZomeFnCall {
            ZomeFnCall::new(
                "test_zome",
                make_cap_request_for_call(context.clone(), token.clone(), "test", "{}"),
                "test",
                "{}",
            )
        }

        let zome_call_from_addr1_bad_token = &ZomeFnCall::new(
            "test_zome",
            make_cap_request_for_call(context.clone(), Address::from("bad token"), "test", "{}"),
            "test",
            "{}",
        );

        let mut cap_functions = CapFunctions::new();
        cap_functions.insert("test_zome".to_string(), vec![String::from("test")]);

        let grant =
            CapTokenGrant::create("foo", CapabilityType::Public, None, cap_functions).unwrap();
        let token = grant.token();
        assert!(verify_grant(
            context.clone(),
            &grant,
            &zome_call_valid(context.clone(), &token)
        ));
        assert!(!verify_grant(
            context.clone(),
            &grant,
            &zome_call_from_addr1_bad_token
        ));

        let mut cap_functions = CapFunctions::new();
        cap_functions.insert("test_zome".to_string(), vec![String::from("other_fn")]);
        let grant_for_other_fn =
            CapTokenGrant::create("foo", CapabilityType::Transferable, None, cap_functions)
                .unwrap();
        assert!(!verify_grant(
            context.clone(),
            &grant_for_other_fn,
            &zome_call_valid(context.clone(), &grant_for_other_fn.token())
        ));

        let mut cap_functions = CapFunctions::new();
        cap_functions.insert("test_zome".to_string(), vec![String::from("test")]);
        let grant = CapTokenGrant::create("foo", CapabilityType::Transferable, None, cap_functions)
            .unwrap();

        let token = grant.token();
        assert!(!verify_grant(
            context.clone(),
            &grant,
            &zome_call_from_addr1_bad_token
        ));

        // call with cap_request for a different function than the zome call
        let zome_call_from_addr1_bad_cap_request = &ZomeFnCall::new(
            "test_zome",
            make_cap_request_for_call(context.clone(), token.clone(), "foo-fn", "{}"),
            "test",
            "{}",
        );
        assert!(!verify_grant(
            context.clone(),
            &grant,
            &zome_call_from_addr1_bad_cap_request
        ));

        assert!(verify_grant(
            context.clone(),
            &grant,
            &zome_call_valid(context.clone(), &token)
        ));
        // should work with same token from a different adddress
        assert!(verify_grant(
            context.clone(),
            &grant,
            &zome_call_valid(context2.clone(), &token)
        ));

        let mut cap_functions = CapFunctions::new();
        cap_functions.insert("test_zome".to_string(), vec![String::from("test")]);
        let grant = CapTokenGrant::create(
            "foo",
            CapabilityType::Assigned,
            Some(vec![test_address1.clone()]),
            cap_functions,
        )
        .unwrap();
        let token = grant.token();
        assert!(!verify_grant(
            context.clone(),
            &grant,
            &zome_call_from_addr1_bad_token
        ));

        // call with cap_request for a different function than the zome call
        let zome_call_from_addr1_bad_cap_request = &ZomeFnCall::new(
            "test_zome",
            make_cap_request_for_call(context.clone(), token.clone(), "foo-fn", "{}"),
            "test",
            "{}",
        );
        assert!(!verify_grant(
            context.clone(),
            &grant,
            &zome_call_from_addr1_bad_cap_request
        ));

        assert!(verify_grant(
            context.clone(),
            &grant,
            &zome_call_valid(context.clone(), &token)
        ));
        // should NOT work with same token from a different adddress
        assert!(!verify_grant(
            context.clone(),
            &grant,
            &zome_call_valid(context2.clone(), &token)
        ));
    }
}
