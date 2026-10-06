use super::EntityId;
use smallvec::SmallVec;
use std::{
    any::TypeId,
    fmt::{self, Display},
    panic::Location,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaseTarget {
    Entity(EntityId),
    Global(TypeId),
}

impl Display for LeaseTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeaseTarget::Entity(entity_id) => write!(f, "#{entity_id}"),
            LeaseTarget::Global(_) => f.write_str("(global)"),
        }
    }
}

struct ActiveLease {
    target: LeaseTarget,
    type_name: &'static str,
    location: &'static Location<'static>,
}

#[derive(Default)]
pub(crate) struct LeaseRegistry(SmallVec<[ActiveLease; 8]>);

impl LeaseRegistry {
    pub(crate) fn acquire(
        &mut self,
        target: LeaseTarget,
        type_name: &'static str,
        location: &'static Location<'static>,
    ) {
        if self.contains(target) {
            self.reentry_panic("update", target, type_name, location)
        }
        self.0.push(ActiveLease {
            target,
            type_name,
            location,
        });
    }

    pub(crate) fn release(&mut self, target: LeaseTarget) {
        let innermost = self.0.last();
        assert!(
            innermost.is_some_and(|innermost| innermost.target == target),
            "ended the update of {target} while the innermost update was {}",
            innermost.map_or_else(
                || "none".to_string(),
                |innermost| format!(
                    "{} {} at {}",
                    innermost.type_name, innermost.target, innermost.location
                )
            ),
        );
        self.0.pop();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn contains(&self, target: LeaseTarget) -> bool {
        self.0.iter().any(|lease| lease.target == target)
    }

    #[cold]
    #[inline(never)]
    #[track_caller]
    pub(crate) fn reentry_panic(
        &self,
        operation: &str,
        target: LeaseTarget,
        type_name: &str,
        location: &'static Location<'static>,
    ) -> ! {
        let existing_lease = self
            .0
            .iter()
            .rfind(|lease| lease.target == target)
            .map_or_else(
                || "none recorded".to_string(),
                |lease| lease.location.to_string(),
            );
        let active_leases = self
            .0
            .iter()
            .map(|lease| {
                format!(
                    "\n  {} {} at {}",
                    lease.type_name, lease.target, lease.location
                )
            })
            .collect::<String>();
        panic!(
            "cannot {operation} {type_name} {target} while it is already being updated\n\
             {operation} at: {location}\n\
             existing update at: {existing_lease}\n\
             active updates, outermost first:{active_leases}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{LeaseRegistry, LeaseTarget};
    use std::{any::TypeId, panic::Location};

    #[test]
    #[should_panic(expected = "while it is already being updated")]
    fn test_acquiring_an_active_target_again_panics() {
        let mut leases = LeaseRegistry::default();
        let target = LeaseTarget::Global(TypeId::of::<()>());

        leases.acquire(target, "()", Location::caller());
        leases.acquire(target, "()", Location::caller());
    }

    #[test]
    #[should_panic(expected = "while the innermost update was")]
    fn test_releasing_a_target_that_is_not_innermost_panics() {
        let mut leases = LeaseRegistry::default();
        let outer = LeaseTarget::Global(TypeId::of::<()>());
        let inner = LeaseTarget::Global(TypeId::of::<u8>());

        leases.acquire(outer, "()", Location::caller());
        leases.acquire(inner, "u8", Location::caller());
        leases.release(outer);
    }
}
