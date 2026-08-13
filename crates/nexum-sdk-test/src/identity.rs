use std::cell::RefCell;

use nexum_sdk::host::{Fault, IdentityHost};
use nexum_sdk::prelude::{Address, Signature};

/// One recorded [`MockIdentity`] signing invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignCall {
    /// Account the guest asked to sign with.
    pub account: Address,
    /// What was signed.
    pub payload: SignPayload,
}

/// The payload of a [`SignCall`], per signing entry point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SignPayload {
    /// A `sign` call: raw message bytes (`personal_sign` semantics).
    Message(Vec<u8>),
    /// A `sign_typed_data` call: the JSON-encoded EIP-712 payload.
    TypedData(String),
}

/// In-memory [`IdentityHost`] with a programmable roster and one signing
/// outcome; records every call. Off-roster accounts fail
/// [`Fault::Denied`]; with no outcome programmed signing fails
/// [`Fault::Unsupported`].
#[derive(Default)]
pub struct MockIdentity {
    accounts: RefCell<Vec<Address>>,
    response: RefCell<Option<Result<Signature, Fault>>>,
    calls: RefCell<Vec<SignCall>>,
}

impl MockIdentity {
    /// Add an account to the roster.
    pub fn add_account(&self, account: Address) {
        self.accounts.borrow_mut().push(account);
    }

    /// Program the outcome every subsequent signing call returns.
    pub fn respond(&self, result: Result<Signature, Fault>) {
        *self.response.borrow_mut() = Some(result);
    }

    /// All signing calls received, in arrival order.
    pub fn calls(&self) -> Vec<SignCall> {
        self.calls.borrow().clone()
    }

    /// Last signing call received, if any.
    pub fn last_call(&self) -> Option<SignCall> {
        self.calls.borrow().last().cloned()
    }

    /// Total signing call count.
    pub fn call_count(&self) -> usize {
        self.calls.borrow().len()
    }

    fn dispatch(&self, account: Address, payload: SignPayload) -> Result<Signature, Fault> {
        self.calls.borrow_mut().push(SignCall { account, payload });
        if !self.accounts.borrow().contains(&account) {
            return Err(Fault::Denied(format!(
                "MockIdentity: account {account} is not held"
            )));
        }
        self.response.borrow().clone().unwrap_or_else(|| {
            Err(Fault::Unsupported(
                "MockIdentity: no signing outcome programmed".to_string(),
            ))
        })
    }
}

impl IdentityHost for MockIdentity {
    fn accounts(&self) -> Result<Vec<Address>, Fault> {
        Ok(self.accounts.borrow().clone())
    }

    fn sign(&self, account: Address, message: &[u8]) -> Result<Signature, Fault> {
        self.dispatch(account, SignPayload::Message(message.to_vec()))
    }

    fn sign_typed_data(&self, account: Address, typed_data: &str) -> Result<Signature, Fault> {
        self.dispatch(account, SignPayload::TypedData(typed_data.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use nexum_sdk::prelude::U256;

    use super::*;

    #[test]
    fn identity_roster_and_programmed_outcome() {
        let identity = MockIdentity::default();
        let account = Address::from([0xAA; 20]);
        assert!(identity.accounts().unwrap().is_empty());
        identity.add_account(account);
        assert_eq!(identity.accounts().unwrap(), vec![account]);

        // No outcome programmed: signing is unsupported, the stub posture.
        let err = identity.sign(account, b"hello").unwrap_err();
        assert!(matches!(err, Fault::Unsupported(ref m) if m.contains("MockIdentity")));

        let signature = Signature::new(U256::from(1), U256::from(2), false);
        identity.respond(Ok(signature));
        assert_eq!(identity.sign(account, b"hello").unwrap(), signature);
        assert_eq!(identity.sign_typed_data(account, "{}").unwrap(), signature);

        assert_eq!(identity.call_count(), 3);
        assert_eq!(
            identity.last_call().unwrap(),
            SignCall {
                account,
                payload: SignPayload::TypedData("{}".to_owned()),
            },
        );
    }

    #[test]
    fn identity_denies_off_roster_accounts() {
        let identity = MockIdentity::default();
        identity.respond(Ok(Signature::new(U256::from(1), U256::from(2), true)));
        let err = identity.sign(Address::from([0xBB; 20]), b"x").unwrap_err();
        assert!(matches!(err, Fault::Denied(_)));
        // The refused call is still recorded.
        assert_eq!(identity.call_count(), 1);
    }
}
