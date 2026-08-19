//! Pass-through [`ComponentBuilder`] wrapping a pre-built backend.

use crate::error::BoxError;
use nexum_runtime_api::{BuilderContext, ComponentBuilder};

/// A [`ComponentBuilder`] that yields a pre-built backend, ignoring the build
/// context. Wrap any mock instance to compose it through the public builder.
pub struct Prebuilt<T>(pub T);

impl<T: Send> ComponentBuilder for Prebuilt<T> {
    type Output = T;

    async fn build(self, _ctx: &BuilderContext<'_>) -> Result<T, BoxError> {
        Ok(self.0)
    }
}
