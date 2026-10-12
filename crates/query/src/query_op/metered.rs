use crate::Result;

/// A generic metered wrapper that holds a result of type `T`.
/// ToDo: Will be replaced by a global metering system.
pub struct Metered<T> {
    pub result: Result<T>,
}
